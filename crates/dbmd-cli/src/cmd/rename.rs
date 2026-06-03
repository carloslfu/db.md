//! `dbmd rename <old> <new>` — move a file + rewrite incoming wiki-links.
//!
//! Thin wrapper target: parse [`RenameArgs`], enforce the `DB.md` frozen-page
//! policy, find every incoming link via `Store::find_links_to` (embedded
//! ripgrep), move the file and rewrite all linkers atomically, then update both
//! affected type-folder indexes write-through (`dbmd_core::index::on_rename`).
//! Report the rewrite count (text or `--json`).
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
/// (5) move the file, then rewrite each linker's `[[old]]` → `[[new]]`;
/// (6) update the moved file's old + new type-folder indexes write-through, then
/// refresh the index entry of every rewritten linker (its indexed frontmatter
/// changed), so the loop path stays byte-identical to a full `index rebuild`;
/// (7) report the rewrite count.
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

    // Move the file: create the destination's parent, then rename.
    if let Some(parent) = new_abs.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::runtime(format!("cannot create destination folder: {e}")))?;
    }
    std::fs::rename(&old_abs, &new_abs)
        .map_err(|e| CliError::runtime(format!("cannot move file: {e}")))?;

    // Rewrite every incoming link. The moved file may itself contain a
    // self-link; it now lives at `<new>`, so include it in the rewrite set.
    // Track the *post-move* store-relative path of every linker we actually
    // rewrote: their indexed frontmatter (e.g. a meeting's `attendees:
    // [[old]]`) just changed on disk, so their `index.jsonl`/`index.md`
    // entries must be refreshed write-through too — otherwise the loop path
    // drifts from a full `index rebuild` (which re-reads the rewritten files).
    let mut rewritten = 0usize;
    let mut rewritten_linkers: Vec<PathBuf> = Vec::new();
    for linker_rel in &linkers {
        // A linker that WAS the old file is now at the new path.
        let linker_rel_now = if linker_rel == &old_rel {
            new_rel.clone()
        } else {
            linker_rel.clone()
        };
        let linker_abs = store.abs_path(&linker_rel_now);
        if rewrite_links_in_file(&linker_abs, &old_rel, &new_rel)? {
            rewritten += 1;
            // A derived index artifact (`index.md` / `index.jsonl`) can legitimately
            // contain `[[old]]` and gets its link text rewritten in place above, but
            // it must NEVER be re-indexed *as content* — `Index::on_write` would
            // catalog the index file as a row in its own type-folder. The catalog
            // owns those files; `on_rename` / `on_write` already keep them current.
            if !is_index_artifact(&linker_rel_now) {
                rewritten_linkers.push(linker_rel_now);
            }
        }
    }

    // Keep both affected type-folder indexes current write-through (the moved
    // file's old + new folders).
    let mut index_warning = index_on_rename(&store, &old_rel, &new_rel);

    // Refresh the index entry of every *other* rewritten linker so its indexed
    // frontmatter reflects the rewritten link. The moved file itself is already
    // handled by `on_rename`; a linker outside any type-folder simply has no
    // entry to refresh (non-fatal, same doctrine as every index write-through).
    for linker in &rewritten_linkers {
        if linker == &new_rel {
            continue;
        }
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

/// Rewrite every `[[old]]` wiki-link in a file to `[[new]]`, delegating the
/// link grammar to [`dbmd_core::graph::rewrite_links_to`] — the write-side twin
/// of the core's backlink parser, so the rewrite recognizes exactly the edges
/// `Store::find_links_to` reported. Returns `true` if the file changed. Reads +
/// writes the raw bytes (not the parser round-trip) so a link inside
/// frontmatter or body is rewritten uniformly and nothing else is reflowed.
fn rewrite_links_in_file(abs: &Path, old_rel: &Path, new_rel: &Path) -> Result<bool, CliError> {
    let text = std::fs::read_to_string(abs)
        .map_err(|e| CliError::runtime(format!("cannot read linker {}: {e}", abs.display())))?;
    let rewritten = dbmd_core::graph::rewrite_links_to(&text, old_rel, new_rel);
    if rewritten == text {
        return Ok(false);
    }
    write_atomic(abs, &rewritten)?;
    Ok(true)
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

/// Atomic write: temp file in the same dir, then rename over the target.
fn write_atomic(path: &Path, contents: &str) -> Result<(), CliError> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dbmd-rename");
    let (mut f, tmp) = create_temp_file(parent, name)?;
    {
        f.write_all(contents.as_bytes())
            .map_err(|e| CliError::runtime(format!("cannot write rewrite: {e}")))?;
        f.sync_all().ok();
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::runtime(format!("cannot finalize rewrite: {e}")));
    }
    sync_parent_dir(parent);
    Ok(())
}

fn create_temp_file(parent: &Path, name: &str) -> Result<(std::fs::File, PathBuf), CliError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for _ in 0..128 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".{name}.tmp.{pid}.{nanos}.{seq}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CliError::runtime(format!("cannot write rewrite: {e}"))),
        }
    }

    Err(CliError::runtime(
        "cannot write rewrite: could not allocate a unique temp file",
    ))
}

fn sync_parent_dir(parent: &Path) {
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
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
