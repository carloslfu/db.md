//! `dbmd rename <old> <new>` — move a file + rewrite incoming wiki-links.
//!
//! Thin wrapper target: parse [`RenameArgs`], enforce the `DB.md` frozen-page
//! policy, find every incoming link via `Store::find_links_to` (embedded
//! ripgrep), rewrite every linker first, move the file only once every rewrite
//! has succeeded, then update both affected type-folder indexes write-through
//! (`dbmd_core::index::on_rename`). Report the rewrite count (text or `--json`).
//!
//! **Failure ordering (no half-renamed store).** The file move is the *last*
//! disk mutation, performed only after every linker rewrite committed. So a
//! rewrite that fails (a non-UTF8 linker, a transient I/O error) leaves the
//! source file in place at `<old>` and every linker still pointing at `<old>` —
//! a self-consistent store, never a moved-file-with-dangling-links half-state.
//! This is not a transaction (no rollback of the linkers already rewritten when
//! a *later* linker fails), but it is **monotone toward consistency**: the only
//! linkers changed before an abort already point at the surviving `<old>` file,
//! and `dbmd index rebuild` reconciles the catalog. A single non-UTF8 linker is
//! skipped (counted as a warning) rather than aborting the whole rename.
//!
//! Wiki-links are full store-relative paths, so an incoming reference to `<old>`
//! is the literal text `[[<old>]]` (optionally `|display`, optionally a trailing
//! `.md`). The link-rewrite grammar lives in the core, beside the backlink
//! parser it mirrors ([`dbmd_core::graph::rewrite_links_to`]): it replaces only
//! the target segment, preserving any display text, and emits the canonical
//! bare `<new>` target — so a library consumer (Obsidian plugin, LSP server)
//! gets the same rename-rewrite this CLI does. This handler only finds the
//! linkers, moves the file, and writes each rewritten linker atomically.

use std::path::{Path, PathBuf};

use crate::cli::RenameArgs;
use crate::cmd::write::{
    core_err, enforce_frozen, index_on_rename, index_on_write, open_store, policy_frozen_error,
    require_store_relative,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd rename`.
///
/// Steps: (1) open the store; (2) refuse if `<old>` (the moved file) or `<new>`
/// (the destination) is a frozen page; (3) refuse if `<old>` is missing or
/// `<new>` already exists; (4) find every incoming linker (embedded ripgrep);
/// (5) rewrite each linker's `[[old]]` → `[[new]]` *while the file still sits at
/// `<old>`*, then move the file last (so a rewrite failure leaves the source in
/// place and the store self-consistent, never half-renamed); (6) re-stamp the
/// moved file's auto-maintained `updated` timestamp (a rename is an edit of that
/// file), skipping gracefully if it has no parseable frontmatter; (7) update the
/// moved file's old + new type-folder indexes write-through, then refresh the
/// index entry of every rewritten linker (its indexed frontmatter changed), so
/// the loop path stays byte-identical to a full `index rebuild`; (8) report the
/// rewrite count.
pub fn run(ctx: &Context, args: &RenameArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    let old_rel = require_store_relative(&store, &args.old)?;
    let new_rel = require_store_relative(&store, &args.new)?;
    let old_abs = store.abs_path(&old_rel);
    let new_abs = store.abs_path(&new_rel);

    if !old_abs.exists() {
        return Err(missing_old_error(&old_rel));
    }
    if new_abs.exists() {
        return Err(dest_exists_error(&new_rel));
    }

    // Policy: `rename` moves a single CONTENT file, rewriting incoming links.
    // It is not a directory-mover and it must never touch the store's reserved
    // meta files. Two guards enforce that invariant before any disk mutation:
    //
    //   1. Reject a directory source. `<old>` exists (checked above) — if it is
    //      a directory, `std::fs::rename` would move the whole subtree, but
    //      `find_links_to(&old_rel)` only matches `[[<old>]]` (the directory
    //      path), which nothing links to, so ZERO inbound links to the moved
    //      *files* get rewritten and both index sidecars drift. Refuse instead.
    //   2. Reject a reserved root meta file as `<old>` OR `<new>`. Moving
    //      `DB.md` out of the root destroys the store (every later command then
    //      fails `NOT_A_STORE`); moving `log.md`/`index.md`/`index.jsonl`, or
    //      landing a content file on top of one of those names, corrupts the
    //      catalog. These files are the catalog's own; `rename` never owns them.
    if old_abs.is_dir() {
        return Err(rename_directory_error(&old_rel));
    }
    if let Some(name) = reserved_meta_name(&old_rel) {
        return Err(reserved_meta_source_error(&old_rel, name));
    }
    if let Some(name) = reserved_meta_name(&new_rel) {
        return Err(reserved_meta_dest_error(&new_rel, name));
    }

    // Policy: refuse moving a frozen page, and refuse landing on a frozen path.
    // Both checks funnel through the one canonical matcher so `rename` enforces
    // frozen pages identically to every other write surface; the destination
    // check recovers the matched entry to name it in its own refusal.
    enforce_frozen(&store, &old_rel)?;
    if let Some(frozen) = store.config.frozen_match(&new_rel) {
        return Err(policy_frozen_error(&frozen));
    }

    // Find every incoming linker BEFORE the move (the on-disk `[[old]]` text is
    // what ripgrep matches). Embedded ripgrep, loop-fast — no whole-store parse.
    let linkers = store.find_links_to(&old_rel).map_err(core_err)?;

    // ── Rewrite every linker FIRST, while the file still lives at `<old>` ─────
    // The move is deferred to AFTER this loop. If a rewrite fails (a non-UTF8
    // linker, a transient I/O error), the source file is still at `<old>` and
    // every linker still references the *existing* `<old>` file — a
    // self-consistent store, not a moved-file-with-dangling-links half-state.
    //
    // The moved file may itself carry a self-link `[[old]]`. It is still at
    // `<old>` here, so its self-link is rewritten to `[[new]]` in place; the
    // deferred move then carries the rewritten file to `<new>`. We track the
    // self-link separately so it is NOT double-counted with `on_rename` below.
    //
    // Track the *post-move* store-relative path of every OTHER rewritten linker:
    // their indexed frontmatter (e.g. a meeting's `attendees: [[old]]`) just
    // changed on disk, so their `index.jsonl`/`index.md` entries must be
    // refreshed write-through too — otherwise the loop path drifts from a full
    // `index rebuild` (which re-reads the rewritten files). A linker that fails
    // to read as UTF-8 is *skipped* (surfaced as a warning) rather than aborting
    // the whole rename: ripgrep's byte-level matcher can report a file whose
    // valid ASCII link line lives beside a stray non-UTF8 byte, and one such
    // externally-dropped source must not break a rename of an unrelated file.
    let mut rewritten = 0usize;
    let mut rewritten_linkers: Vec<PathBuf> = Vec::new();
    let mut skip_warnings: Vec<String> = Vec::new();
    for linker_rel in &linkers {
        // The linker is rewritten at its CURRENT path (`<old>` for a self-link),
        // because the move has not happened yet.
        let linker_abs = store.abs_path(linker_rel);
        match rewrite_links_in_file(&linker_abs, &old_rel, &new_rel) {
            Ok(true) => {
                rewritten += 1;
                // The self-link (the moved file itself) is handled by
                // `on_rename` below — do not queue it as an `on_write` too.
                // A derived index artifact (`index.md` / `index.jsonl`) can
                // legitimately contain `[[old]]` and gets its link text
                // rewritten in place above, but it must NEVER be re-indexed *as
                // content* — `Index::on_write` would catalog the index file as a
                // row in its own type-folder. The catalog owns those files;
                // `on_rename` / `on_write` already keep them current.
                if linker_rel != &old_rel && !is_index_artifact(linker_rel) {
                    rewritten_linkers.push(linker_rel.clone());
                }
            }
            Ok(false) => {}
            Err(RewriteError::NotUtf8) => {
                // A non-UTF8 linker: skip it, surface a warning, keep going.
                // The rename still completes; this one stray file keeps its
                // `[[old]]` text and a `dbmd validate` flags the dangling link.
                skip_warnings.push(format!(
                    "skipped non-UTF8 linker {} (its `[[{}]]` link was not rewritten)",
                    path_to_unix(linker_rel),
                    path_to_unix(&old_rel)
                ));
            }
            // A real I/O failure (not a UTF-8 issue) aborts BEFORE the move, so
            // the source survives at `<old>` and the store stays consistent.
            Err(RewriteError::Io(e)) => return Err(e),
        }
    }

    // ── Move the file LAST, only after every rewrite committed ───────────────
    // Create the destination's parent, then rename. Reaching here means no
    // linker rewrite hard-failed, so the move cannot strand a dangling link.
    if let Some(parent) = new_abs.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::runtime(format!("cannot create destination folder: {e}")))?;
    }
    std::fs::rename(&old_abs, &new_abs)
        .map_err(|e| CliError::runtime(format!("cannot move file: {e}")))?;

    // Re-stamp the MOVED file's auto-maintained `updated` to now: a rename IS an
    // edit of that file (its path changed), so its recency must reflect the move
    // the same way `write` seeds it on create and `fm set` bumps it on edit.
    // Done BEFORE `index_on_rename` so the index picks up the new timestamp. If
    // `new_abs` has no parseable frontmatter (a non-content file with no `---`
    // block), `read_file` errors and we skip the re-stamp gracefully rather than
    // failing the rename. We deliberately do NOT cascade the bump to the
    // link-rewritten linker files: renaming a link target must not mark every
    // linking record as freshly edited (that would pollute recency ordering).
    if let Ok((mut fm, body)) = dbmd_core::parser::read_file(&new_abs) {
        fm.updated = Some(dbmd_core::now());
        dbmd_core::parser::write_file(&new_abs, &fm, &body).map_err(core_err)?;
    }

    // Keep both affected type-folder indexes current write-through (the moved
    // file's old + new folders).
    let mut index_warning = index_on_rename(&store, &old_rel, &new_rel);

    // Surface any skipped non-UTF8 linkers as a (non-fatal) warning, preferring
    // an index warning if one already exists so the most actionable line shows.
    if let Some(w) = skip_warnings.into_iter().next() {
        index_warning.get_or_insert(w);
    }

    // Refresh the index entry of every *other* rewritten linker so its indexed
    // frontmatter reflects the rewritten link. The moved file itself is already
    // excluded from `rewritten_linkers` (it is handled by `on_rename`); each
    // remaining linker never moved, so its store-relative path is unchanged by
    // the rename. A linker outside any type-folder simply has no entry to refresh
    // (non-fatal, same doctrine as every index write-through).
    for linker in &rewritten_linkers {
        if let Some(w) = index_on_write(&store, linker) {
            index_warning.get_or_insert(w);
        }
    }

    emit_result(
        ctx,
        &path_to_unix(&old_rel),
        &path_to_unix(&new_rel),
        rewritten,
        &index_warning,
    );
    Ok(())
}

/// Outcome of a single linker rewrite that the caller must distinguish.
///
/// A non-UTF8 linker is *recoverable* — the rename skips it and warns — so it is
/// a distinct variant from a genuine I/O failure (which aborts the rename before
/// the file move, leaving the store self-consistent). Modeling the two kinds
/// separately is what lets the loop in [`run`] not abort the whole rename on a
/// stray non-UTF8 byte that ripgrep matched but `read_to_string` rejects.
#[derive(Debug)]
enum RewriteError {
    /// The linker is not valid UTF-8 (`io::ErrorKind::InvalidData` from
    /// `read_to_string`). Recoverable: skip the file, do not abort the rename.
    NotUtf8,
    /// A genuine I/O failure (permissions, removed file, write error). Fatal to
    /// the rename — but it aborts *before* the file move, so the store stays
    /// consistent.
    Io(CliError),
}

/// Rewrite every `[[old]]` wiki-link in a file to `[[new]]`, delegating the
/// link grammar to [`dbmd_core::graph::rewrite_links_to`] — the write-side twin
/// of the core's backlink parser, so the rewrite recognizes exactly the edges
/// `Store::find_links_to` reported. Returns `Ok(true)` if the file changed,
/// `Ok(false)` for a no-op. Reads + writes the raw bytes (not the parser
/// round-trip) so a link inside frontmatter or body is rewritten uniformly and
/// nothing else is reflowed.
///
/// A read that fails because the bytes are not UTF-8 returns
/// [`RewriteError::NotUtf8`] (recoverable — the caller skips this linker) rather
/// than a fatal error: `find_links_to`'s byte-level ripgrep matcher can report a
/// file whose valid ASCII `[[old]]` line sits beside a stray non-UTF8 byte, and
/// one such externally-dropped source must not abort an otherwise-clean rename.
/// Every other read/write failure is a genuine [`RewriteError::Io`].
fn rewrite_links_in_file(abs: &Path, old_rel: &Path, new_rel: &Path) -> Result<bool, RewriteError> {
    let text = match std::fs::read_to_string(abs) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(RewriteError::NotUtf8);
        }
        Err(e) => {
            return Err(RewriteError::Io(CliError::runtime(format!(
                "cannot read linker {}: {e}",
                abs.display()
            ))));
        }
    };
    let rewritten = dbmd_core::graph::rewrite_links_to(&text, old_rel, new_rel);
    if rewritten == text {
        return Ok(false);
    }
    write_atomic(abs, &rewritten).map_err(RewriteError::Io)?;
    Ok(true)
}

/// The reserved meta-file basenames `rename` must never move (as source) or
/// land on (as destination). `DB.md` is the store marker — moving it out of the
/// root destroys the store. `log.md` / `index.md` / `index.jsonl` are the
/// catalog's own derived files; the index machinery owns them, so a rename must
/// not relocate one or clobber another file onto one of these names.
const RESERVED_META_BASENAMES: [&str; 4] = ["DB.md", "log.md", "index.md", "index.jsonl"];

/// The reserved meta-file basename a store-relative path carries, if any —
/// matched on the final path component (case-sensitive, the same spelling the
/// content walks skip). Returns `None` for an ordinary content path.
fn reserved_meta_name(rel: &Path) -> Option<&'static str> {
    let name = rel.file_name().and_then(|n| n.to_str())?;
    RESERVED_META_BASENAMES
        .into_iter()
        .find(|reserved| *reserved == name)
}

/// Structured error: `<old>` is a directory (exit `4`, policy refusal). `rename`
/// moves a single content file and rewrites incoming links to it; a directory
/// source would relocate the whole subtree while leaving every inbound link to
/// the contained files dangling and both index sidecars stale.
fn rename_directory_error(old: &Path) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "RENAME_NOT_A_FILE",
        format!(
            "rename refused: `{}` is a directory; `dbmd rename` moves one content file at a time",
            path_to_unix(old)
        ),
    )
    .with_hint("rename the individual files inside it, or move the folder with your shell + run `dbmd index rebuild`")
}

/// Structured error: `<old>` is a reserved meta file (exit `4`, policy refusal).
/// Moving `DB.md` destroys the store; moving `log.md`/`index.md`/`index.jsonl`
/// corrupts the catalog. The index machinery owns these files.
fn reserved_meta_source_error(old: &Path, name: &str) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "RENAME_RESERVED_META",
        format!(
            "rename refused: `{}` is a reserved db.md meta file ({name}) and cannot be renamed",
            path_to_unix(old)
        ),
    )
    .with_hint(
        "`DB.md`/`log.md`/`index.md`/`index.jsonl` are managed by db.md; never move them by hand",
    )
}

/// Structured error: `<new>` would land on a reserved meta-file name (exit `4`,
/// policy refusal). A content file must never be renamed onto `DB.md`,
/// `log.md`, `index.md`, or `index.jsonl` — it would masquerade as catalog
/// machinery and corrupt the index.
fn reserved_meta_dest_error(new: &Path, name: &str) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "RENAME_RESERVED_META",
        format!(
            "rename refused: destination `{}` uses the reserved db.md meta-file name `{name}`",
            path_to_unix(new)
        ),
    )
    .with_hint("choose a destination filename that is not a db.md meta file")
}

/// Structured error: `<old>` doesn't exist (exit `1`).
fn missing_old_error(old: &Path) -> CliError {
    CliError::runtime(format!(
        "cannot rename `{}`: file does not exist",
        path_to_unix(old)
    ))
}

/// Structured error: `<new>` already exists (exit `5`, a collision). Refusing
/// keeps `rename` from silently clobbering an existing file.
fn dest_exists_error(new: &Path) -> CliError {
    CliError::new(
        ExitCode::Collision,
        "PATH_COLLISION",
        format!("destination `{}` already exists", path_to_unix(new)),
    )
    .with_hint("choose a destination that does not exist, or remove/merge the existing file first")
}

/// Emit the result: a human summary line, or a `--json` object with the move +
/// rewrite count. Non-fatal index warning to stderr.
fn emit_result(
    ctx: &Context,
    old: &str,
    new: &str,
    rewritten: usize,
    index_warning: &Option<String>,
) {
    if let Some(w) = index_warning {
        eprintln!("dbmd: warning: {w}");
    }
    if ctx.json {
        let out = serde_json::json!({
            "renamed": { "from": old, "to": new },
            "links_rewritten": rewritten,
        });
        println!("{out}");
    } else {
        let files = if rewritten == 1 { "file" } else { "files" };
        println!("renamed {old} -> {new} ({rewritten} {files} rewritten)");
    }
}

/// Atomic, durable write of a rewritten content file — delegates to the one
/// core primitive (`dbmd_core::write_atomic`: temp + fsync + rename +
/// parent-fsync). A rewritten linker is primary data, so it uses the durable
/// path, same as the original `dbmd write`.
fn write_atomic(path: &Path, contents: &str) -> Result<(), CliError> {
    dbmd_core::write_atomic(path, contents.as_bytes())
        .map_err(|e| CliError::runtime(format!("cannot finalize rewrite: {e}")))
}

/// True for a derived index artifact (`index.md` / `index.jsonl`). The catalog
/// owns these; a rename rewrites any `[[old]]` text inside them in place, but
/// they are never re-indexed as content rows.
fn is_index_artifact(p: &Path) -> bool {
    matches!(
        p.file_name().and_then(|n| n.to_str()),
        Some("index.md") | Some("index.jsonl")
    )
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

    /// The pure wiki-link rewrite grammar (plain / display / `.md` / prefix
    /// boundaries / multiple occurrences) is owned and tested in
    /// `dbmd_core::graph::rewrite_links_to`. These CLI tests cover only the
    /// handler-side file wrapper that the core does NOT: read the bytes,
    /// delegate to core, short-circuit a no-op, and atomic-write on a change.

    #[test]
    fn rewrite_links_in_file_retargets_and_persists_via_core() {
        let tmp = std::env::temp_dir().join(format!("dbmd-rename-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("linker.md");
        std::fs::write(
            &f,
            "Met [[records/contacts/sarah.md|Sarah]] and [[records/contacts/sarah-2]].",
        )
        .unwrap();

        let changed = rewrite_links_in_file(
            &f,
            Path::new("records/contacts/sarah"),
            Path::new("records/contacts/sarah-chen"),
        )
        .unwrap();
        assert!(changed, "a matching link must report a change");

        // The file on disk now carries the canonical bare new target with the
        // display preserved; the prefix-collision link is untouched — exactly
        // the core grammar, observed through the handler's read/write wrapper.
        let after = std::fs::read_to_string(&f).unwrap();
        assert_eq!(
            after,
            "Met [[records/contacts/sarah-chen|Sarah]] and [[records/contacts/sarah-2]]."
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A throwaway store: a `TempDir` with a parseable `DB.md` marker, matching
    /// the DB.md shape the rename integration suite uses with `open_strict`.
    fn make_store() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: T\n---\n\n# Store\n",
        )
        .unwrap();
        dir
    }

    fn rename_args(old: &str, new: &str, dir: &Path) -> RenameArgs {
        RenameArgs {
            old: old.to_string(),
            new: new.to_string(),
            dir: dir.to_str().unwrap().to_string(),
        }
    }

    fn ctx() -> Context {
        Context {
            json: false,
            color: crate::context::ColorChoice::default(),
        }
    }

    #[test]
    fn reserved_meta_name_matches_only_root_meta_files() {
        // Exact reserved basenames are recognized regardless of folder depth.
        assert_eq!(reserved_meta_name(Path::new("DB.md")), Some("DB.md"));
        assert_eq!(reserved_meta_name(Path::new("log.md")), Some("log.md"));
        assert_eq!(
            reserved_meta_name(Path::new("records/notes/index.md")),
            Some("index.md")
        );
        assert_eq!(
            reserved_meta_name(Path::new("records/notes/index.jsonl")),
            Some("index.jsonl")
        );
        // An ordinary content file is not reserved (substring/prefix don't count).
        assert_eq!(reserved_meta_name(Path::new("records/notes/n.md")), None);
        assert_eq!(
            reserved_meta_name(Path::new("records/notes/DB-old.md")),
            None
        );
        assert_eq!(reserved_meta_name(Path::new("records/db.md")), None); // case-sensitive
    }

    #[test]
    fn rename_refuses_to_move_db_md_meta_marker() {
        // The store-destroying case: `dbmd rename DB.md records/notes/moved.md`.
        // Must refuse with a policy error and leave `DB.md` in place.
        let dir = make_store();
        let args = rename_args("DB.md", "records/notes/moved.md", dir.path());
        let err = run(&ctx(), &args).unwrap_err();
        assert_eq!(err.exit, ExitCode::Policy);
        assert_eq!(err.code, "RENAME_RESERVED_META");
        // DB.md survives; the store is intact.
        assert!(
            dir.path().join("DB.md").exists(),
            "the store marker must not be moved"
        );
        assert!(
            !dir.path().join("records/notes/moved.md").exists(),
            "nothing must be written to the destination"
        );
    }

    #[test]
    fn rename_refuses_a_directory_source() {
        // The store-corrupting case: `dbmd rename records/vendors records/suppliers`
        // where `records/vendors` is a directory. Must refuse with a policy error
        // and leave the directory (and its files) untouched.
        let dir = make_store();
        let vendors = dir.path().join("records/vendors");
        std::fs::create_dir_all(&vendors).unwrap();
        std::fs::write(
            vendors.join("v1.md"),
            "---\ntype: vendor\nsummary: V\n---\n# V\n",
        )
        .unwrap();

        let args = rename_args("records/vendors", "records/suppliers", dir.path());
        let err = run(&ctx(), &args).unwrap_err();
        assert_eq!(err.exit, ExitCode::Policy);
        assert_eq!(err.code, "RENAME_NOT_A_FILE");
        // The source directory and its file survive; nothing moved.
        assert!(
            vendors.join("v1.md").exists(),
            "the directory must be untouched"
        );
        assert!(
            !dir.path().join("records/suppliers").exists(),
            "no destination directory must be created"
        );
    }

    #[test]
    fn rename_refuses_landing_on_a_reserved_meta_name() {
        // A content file must never be renamed onto a reserved meta-file name.
        let dir = make_store();
        let src = dir.path().join("records/notes/n.md");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "---\ntype: note\nsummary: N\n---\n# N\n").unwrap();

        let args = rename_args("records/notes/n.md", "records/notes/index.md", dir.path());
        let err = run(&ctx(), &args).unwrap_err();
        assert_eq!(err.exit, ExitCode::Policy);
        assert_eq!(err.code, "RENAME_RESERVED_META");
        // The source survives; nothing landed on the reserved name.
        assert!(src.exists(), "the source content file must not be moved");
    }

    #[test]
    fn rewrite_links_in_file_is_a_no_op_when_no_link_matches() {
        let tmp = std::env::temp_dir().join(format!("dbmd-rename-noop-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("linker.md");
        let original = "Only [[wiki/topics/elsewhere]] here.";
        std::fs::write(&f, original).unwrap();

        let changed = rewrite_links_in_file(
            &f,
            Path::new("records/contacts/sarah"),
            Path::new("records/contacts/sarah-chen"),
        )
        .unwrap();
        assert!(!changed, "no matching link → no change reported");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            original,
            "a no-op must leave the file byte-for-byte unchanged"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
