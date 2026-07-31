//! `dbmd rename <old> <new>` — move a file + rewrite incoming wiki-links.
//!
//! Thin wrapper target: parse [`RenameArgs`], enforce the `DB.md` frozen-page
//! policy, find every incoming link via `Store::find_links_to` (embedded
//! ripgrep), prepare every changed byte, then commit through a durable
//! forward-recovery journal. Finally update both affected type-folder indexes
//! write-through (`dbmd_core::index::on_rename`) and report the rewrite count
//! (text or `--json`).
//!
//! **Crash consistency.** The command stages the renamed source and every linker
//! rewrite beneath `.dbmd/rename-transactions/`, then durably claims one
//! `.dbmd/rename-transaction.json` before publishing authored bytes. Commit
//! installs `<new>` while `<old>` still exists, switches each linker, removes
//! `<old>`, refreshes derived indexes, and clears the journal last. Thus every
//! intermediate state resolves both old and new link targets, and a retry
//! validates the journal's paths and idempotently converges the exact same
//! transaction. A destination race is accepted only when its bytes exactly
//! match the staged source. Invalid UTF-8 outside a link target is preserved
//! byte-for-byte while the target itself is retargeted.
//!
//! Wiki-links are full store-relative paths, so an incoming reference to `<old>`
//! is the literal text `[[<old>]]` (optionally `|display`, optionally a trailing
//! `.md`). The link-rewrite grammar lives in the core, beside the backlink
//! parser it mirrors ([`dbmd_core::graph::rewrite_links_to`]): it replaces only
//! the target segment, preserving any display text, and emits the canonical
//! bare `<new>` target — so a library consumer (Obsidian plugin, LSP server)
//! gets the same rename-rewrite this CLI does. This handler finds the linkers,
//! stages the resulting bytes, and commits them through descriptor-relative
//! no-follow writes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::RenameArgs;
use crate::cmd::write::{
    core_err, enforce_frozen, index_on_rename, index_on_write, open_store, policy_frozen_error,
    require_store_relative,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

/// Run `dbmd rename`.
///
/// Steps: (1) open the store; (2) refuse if `<old>` (the moved file) or `<new>`
/// (the destination) is a frozen page; (3) refuse if `<old>` is missing or
/// `<new>` already exists; (4) find every incoming linker (embedded ripgrep);
/// (5) stage the renamed source (including its new `updated` timestamp) and
/// every linker rewrite; (6) durably journal and commit destination, linkers,
/// old-source removal, and derived indexes in forward-recoverable order; (7)
/// report the rewrite count.
pub fn run(ctx: &Context, args: &RenameArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let _transaction = store.transaction().map_err(CliError::from)?;

    let old_rel = require_store_relative(&store, &args.old)?;
    let new_rel = require_store_relative(&store, &args.new)?;
    if let Some(recovered) = recover_pending_rename(&store)? {
        if recovered.old == old_rel && recovered.new == new_rel {
            emit_result(
                ctx,
                &path_to_unix(&old_rel),
                &path_to_unix(&new_rel),
                recovered.rewritten,
                &recovered.index_warning,
            );
            return Ok(());
        }
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
    if rename_path_probe(&old_rel, store.directory_exists(&old_rel))? {
        return Err(rename_directory_error(&old_rel));
    }
    if !store
        .regular_file_exists(&old_rel)
        .map_err(|error| rename_path_error(&old_rel, error))?
    {
        return Err(missing_old_error(&old_rel));
    }
    let destination_is_regular = store
        .regular_file_exists(&new_rel)
        .map_err(|error| rename_path_error(&new_rel, error))?;
    if destination_is_regular || rename_path_probe(&new_rel, store.directory_exists(&new_rel))? {
        return Err(dest_exists_error(&new_rel));
    }
    if let Some(name) = reserved_meta_name(&old_rel) {
        return Err(reserved_meta_source_error(&old_rel, name));
    }
    if let Some(name) = reserved_meta_name(&new_rel) {
        return Err(reserved_meta_dest_error(&new_rel, name));
    }
    if !dbmd_core::store::is_content_path(&old_rel) || !dbmd_core::store::is_content_path(&new_rel)
    {
        return Err(CliError::new(
            ExitCode::Policy,
            "RENAME_NOT_CONTENT",
            "rename requires both paths to live under sources/ or records/",
        ));
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

    // Prepare every authored byte change before publishing any of them. The
    // durable journal makes commit forward-recoverable: destination is claimed
    // while the old file still exists, then linkers are replaced, then old is
    // removed. A crash at any instruction leaves enough staged bytes for the
    // next `dbmd rename` to finish the exact same transaction.
    let mut rewritten = 0usize;
    let mut rewritten_linkers: Vec<PathBuf> = Vec::new();
    let mut skip_warnings: Vec<String> = Vec::new();
    // The owned backlink scan correctly prunes external aliases before it can
    // read their targets. Preserve operator visibility by naming the first
    // ignored alias without claiming its target contained a matching link.
    if let Some(path) = store.unowned_symlinks().map_err(core_err)?.first() {
        skip_warnings.push(format!(
            "ignored unowned symlink {} (its target is outside this store's ownership boundary)",
            path_to_unix(path)
        ));
    }
    let transaction_id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let stage_root = PathBuf::from(".dbmd")
        .join("rename-transactions")
        .join(&transaction_id);
    let source_stage = stage_root.join("source.stage");

    let source_original = store
        .read_bounded(&old_rel, dbmd_core::parser::MAX_DBMD_FILE_BYTES)
        .map_err(|error| CliError::runtime(format!("cannot read source: {error}")))?;
    let (source_bytes, source_link_rewritten) =
        prepare_source_bytes(&source_original, &old_rel, &new_rel);
    if source_link_rewritten {
        rewritten += 1;
    }
    store
        .write_atomic_new(&source_stage, &source_bytes)
        .map_err(|error| CliError::runtime(format!("cannot stage renamed source: {error}")))?;

    let mut writes = Vec::new();
    for linker_rel in &linkers {
        // ── Layer guard: rename only rewrites CONTENT files ──────────────────
        // `find_links_to` rides `Store::find_links_to_any`, whose scan
        // (`walk_all_md`) walks from the store ROOT and so reports `[[old]]` text
        // wherever it lives — store-root files (`NOTES.md`), non-layer dirs
        // (`scratch/`, `EXPECTED/` test goldens, `archive/` frozen copies), and
        // `index.md` catalogs alike. Those are NOT db.md content (SPEC § content
        // files = everything under `sources/` and `records/` ONLY); `rename` does
        // not own their bytes and must never rewrite them in place — that is the
        // silent-mutation / data-loss bug this guard closes. The sibling
        // `graph backlinks` already filters the same scan through the content
        // predicate and correctly ignores these files; here we make the *mutating*
        // surface agree, via the same canonical predicate
        // (`dbmd_core::store::is_content_path`, the first-component layer check
        // the graph engine's `is_content_rel` uses). The read-only working-set
        // validate scan (`find_links_to_any`) is untouched — the filter lives at
        // rename's point of mutation. The moved file's own self-link
        // (`linker_rel == old_rel`) is exempt: it is rewritten in place and then
        // carried to `<new>` by the deferred move, so a rename of a (rare)
        // non-content `<old>` still self-retargets correctly.
        if linker_rel != &old_rel && !dbmd_core::store::is_content_path(linker_rel) {
            continue;
        }
        if linker_rel == &old_rel {
            continue;
        }
        match prepare_link_rewrite(&store, linker_rel, &old_rel, &new_rel) {
            Ok(Some(plan)) => {
                // Count only real authored rewrites toward the user-facing "N
                // files rewritten" total. A derived index artifact (`index.md` /
                // `index.jsonl`) can legitimately contain `[[old]]` and gets its
                // link text rewritten in place above, but it is regenerated
                // write-through by `on_rename` below — counting it would inflate
                // the total with a catalog the operator never authored. The
                // self-link (the moved file itself) IS a real edit and stays
                // counted; it is only excluded from the re-index queue.
                if !is_index_artifact(linker_rel) {
                    rewritten += 1;
                }
                // The self-link (the moved file itself) is handled by
                // `on_rename` below — do not queue it as an `on_write` too.
                // A derived index artifact must NEVER be re-indexed *as content*
                // — `Index::on_write` would catalog the index file as a row in
                // its own type-folder. The catalog owns those files; `on_rename`
                // / `on_write` already keep them current.
                if linker_rel != &old_rel && !is_index_artifact(linker_rel) {
                    rewritten_linkers.push(linker_rel.clone());
                }
                let stage = stage_root.join(format!("linker-{:08}.stage", writes.len()));
                store
                    .write_atomic_new(&stage, &plan.after)
                    .map_err(|error| {
                        CliError::runtime(format!(
                            "cannot stage linker {}: {error}",
                            linker_rel.display()
                        ))
                    })?;
                writes.push(JournalWrite {
                    target: linker_rel.clone(),
                    stage,
                    before: fingerprint(&plan.before),
                    after: fingerprint(&plan.after),
                });
            }
            Ok(None) => {}
            Err(RewriteError::Io(e)) => return Err(e),
        }
    }

    let journal = RenameJournal {
        version: 2,
        transaction_id,
        old: old_rel.clone(),
        new: new_rel.clone(),
        source_stage,
        source_before: fingerprint(&source_original),
        source_after: fingerprint(&source_bytes),
        writes,
        rewritten,
        rewritten_linkers: rewritten_linkers.clone(),
    };
    let journal_bytes = serde_json::to_vec(&journal)
        .map_err(|error| CliError::runtime(format!("cannot encode rename journal: {error}")))?;
    store
        .write_atomic_new(Path::new(RENAME_JOURNAL), &journal_bytes)
        .map_err(|error| {
            CliError::runtime(format!(
                "cannot durably claim the rename transaction: {error}"
            ))
        })?;
    let mut index_warning = apply_rename_journal(&store, &journal, None)?;

    // Surface an ignored unowned symlink as a non-fatal warning, preferring an
    // index warning if one already exists so the most actionable line shows.
    if let Some(w) = skip_warnings.into_iter().next() {
        index_warning.get_or_insert(w);
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

fn rename_path_probe(path: &Path, result: std::io::Result<bool>) -> Result<bool, CliError> {
    result.map_err(|error| rename_path_error(path, error))
}

fn rename_path_error(path: &Path, error: std::io::Error) -> CliError {
    CliError::new(
        ExitCode::Policy,
        "PATH_OUTSIDE_STORE",
        format!(
            "rename path {} may resolve outside the store's safe ownership boundary: {error}",
            path_to_unix(path)
        ),
    )
}

const RENAME_JOURNAL: &str = ".dbmd/rename-transaction.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalWrite {
    target: PathBuf,
    stage: PathBuf,
    before: FileFingerprint,
    after: FileFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenameJournal {
    version: u32,
    transaction_id: String,
    old: PathBuf,
    new: PathBuf,
    source_stage: PathBuf,
    source_before: FileFingerprint,
    source_after: FileFingerprint,
    writes: Vec<JournalWrite>,
    rewritten: usize,
    rewritten_linkers: Vec<PathBuf>,
}

#[derive(Debug)]
struct RecoveredRename {
    old: PathBuf,
    new: PathBuf,
    rewritten: usize,
    index_warning: Option<String>,
}

fn recover_pending_rename(store: &dbmd_core::Store) -> Result<Option<RecoveredRename>, CliError> {
    let bytes = match store.read_bounded(Path::new(RENAME_JOURNAL), 4 * 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CliError::runtime(format!(
                "cannot read pending rename journal: {error}"
            )))
        }
    };
    let journal: RenameJournal = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::runtime(format!("invalid pending rename journal: {error}")))?;
    validate_journal(&journal)?;
    let index_warning = apply_rename_journal(store, &journal, None)?;
    Ok(Some(RecoveredRename {
        old: journal.old,
        new: journal.new,
        rewritten: journal.rewritten,
        index_warning,
    }))
}

fn validate_journal(journal: &RenameJournal) -> Result<(), CliError> {
    let expected_root = PathBuf::from(".dbmd")
        .join("rename-transactions")
        .join(&journal.transaction_id);
    let valid_stage = |stage: &Path| {
        stage.starts_with(&expected_root)
            && !stage.is_absolute()
            && !stage
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
    };
    let valid_fingerprint = |fingerprint: &FileFingerprint| {
        fingerprint.sha256.len() == 64
            && fingerprint
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    let mut targets = std::collections::BTreeSet::new();
    if journal.version != 2
        || journal.transaction_id.is_empty()
        || journal.old == journal.new
        || !dbmd_core::store::is_content_path(&journal.old)
        || !dbmd_core::store::is_content_path(&journal.new)
        || !valid_stage(&journal.source_stage)
        || !valid_fingerprint(&journal.source_before)
        || !valid_fingerprint(&journal.source_after)
        || journal.writes.iter().any(|write| {
            !dbmd_core::store::is_content_path(&write.target)
                || write.target == journal.old
                || write.target == journal.new
                || !targets.insert(write.target.clone())
                || !valid_stage(&write.stage)
                || !valid_fingerprint(&write.before)
                || !valid_fingerprint(&write.after)
                || write.before == write.after
        })
    {
        return Err(CliError::new(
            ExitCode::Runtime,
            "RENAME_JOURNAL_INVALID",
            "pending rename journal contains an unsafe path or unsupported version",
        ));
    }
    Ok(())
}

fn fingerprint(bytes: &[u8]) -> FileFingerprint {
    FileFingerprint {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    }
}

fn read_optional(store: &dbmd_core::Store, path: &Path) -> Result<Option<Vec<u8>>, CliError> {
    match store.read_bounded(path, dbmd_core::parser::MAX_DBMD_FILE_BYTES) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(rename_conflict(
            path,
            format!("path cannot be safely read: {error}"),
        )),
    }
}

fn matches_fingerprint(bytes: &[u8], expected: &FileFingerprint) -> bool {
    bytes.len() as u64 == expected.bytes && fingerprint(bytes).sha256 == expected.sha256
}

fn rename_conflict(path: &Path, detail: impl Into<String>) -> CliError {
    CliError::new(
        ExitCode::Collision,
        "RENAME_CONFLICT",
        format!(
            "rename recovery refused: `{}` changed outside this transaction ({})",
            path_to_unix(path),
            detail.into()
        ),
    )
    .with_hint(
        "inspect the pending rename journal and conflicting file; restore either the original or already-applied bytes before retrying",
    )
}

/// Validate every authored path and every immutable stage before the first
/// mutation. Recovery accepts only the transaction's exact before state or its
/// exact already-applied state. Any third state fails closed with
/// `RENAME_CONFLICT`, leaving every authored file untouched.
fn preflight_rename_journal(
    store: &dbmd_core::Store,
    journal: &RenameJournal,
) -> Result<(), CliError> {
    let source_stage = store
        .read_bounded(
            &journal.source_stage,
            dbmd_core::parser::MAX_DBMD_FILE_BYTES,
        )
        .map_err(|error| {
            rename_conflict(
                &journal.source_stage,
                format!("source stage is missing or unreadable: {error}"),
            )
        })?;
    if !matches_fingerprint(&source_stage, &journal.source_after) {
        return Err(rename_conflict(
            &journal.source_stage,
            "source stage does not match its journal digest",
        ));
    }

    let old = read_optional(store, &journal.old)?;
    let new = read_optional(store, &journal.new)?;
    let old_is_before = old
        .as_deref()
        .is_some_and(|bytes| matches_fingerprint(bytes, &journal.source_before));
    let new_is_after = new
        .as_deref()
        .is_some_and(|bytes| matches_fingerprint(bytes, &journal.source_after));
    let source_state_is_valid = match (old.as_ref(), new.as_ref()) {
        (Some(_), None) => old_is_before,
        (Some(_), Some(_)) => old_is_before && new_is_after,
        (None, Some(_)) => new_is_after,
        (None, None) => false,
    };
    if !source_state_is_valid {
        let path = if old.as_ref().is_some_and(|_| !old_is_before) {
            &journal.old
        } else {
            &journal.new
        };
        return Err(rename_conflict(
            path,
            "source/destination bytes are neither the original nor the already-applied state",
        ));
    }

    for write in &journal.writes {
        let staged = store
            .read_bounded(&write.stage, dbmd_core::parser::MAX_DBMD_FILE_BYTES)
            .map_err(|error| {
                rename_conflict(
                    &write.stage,
                    format!("linker stage is missing or unreadable: {error}"),
                )
            })?;
        if !matches_fingerprint(&staged, &write.after) {
            return Err(rename_conflict(
                &write.stage,
                "linker stage does not match its journal digest",
            ));
        }
        let current = read_optional(store, &write.target)?.ok_or_else(|| {
            rename_conflict(&write.target, "linker was removed during the transaction")
        })?;
        if !matches_fingerprint(&current, &write.before)
            && !matches_fingerprint(&current, &write.after)
        {
            return Err(rename_conflict(
                &write.target,
                "linker bytes are neither the original nor the already-applied state",
            ));
        }
    }
    Ok(())
}

/// Finish one durable rename transaction. `fail_after` is deterministic fault
/// injection for tests: an error after any commit step leaves the journal and
/// stages intact, and a later call with `None` must converge to all-new.
fn apply_rename_journal(
    store: &dbmd_core::Store,
    journal: &RenameJournal,
    fail_after: Option<usize>,
) -> Result<Option<String>, CliError> {
    validate_journal(journal)?;
    preflight_rename_journal(store, journal)?;
    let mut step = 0usize;
    let maybe_fail = |step: usize| -> Result<(), CliError> {
        if fail_after == Some(step) {
            Err(CliError::runtime(format!(
                "injected rename failure after commit step {step}"
            )))
        } else {
            Ok(())
        }
    };

    let source = store
        .read_bounded(
            &journal.source_stage,
            dbmd_core::parser::MAX_DBMD_FILE_BYTES,
        )
        .map_err(|error| CliError::runtime(format!("cannot read staged source: {error}")))?;
    match read_optional(store, &journal.new)? {
        Some(existing) if matches_fingerprint(&existing, &journal.source_after) => {}
        Some(_) => {
            return Err(rename_conflict(
                &journal.new,
                "destination changed after transaction preflight",
            ))
        }
        None => match store.write_atomic_new(&journal.new, &source) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_optional(store, &journal.new)?.ok_or_else(|| {
                    rename_conflict(&journal.new, "destination raced into and out of existence")
                })?;
                if !matches_fingerprint(&existing, &journal.source_after) {
                    return Err(rename_conflict(
                        &journal.new,
                        "destination was concurrently created with different bytes",
                    ));
                }
            }
            Err(error) => {
                return Err(CliError::runtime(format!(
                    "cannot install rename destination: {error}"
                )))
            }
        },
    }
    step += 1;
    maybe_fail(step)?;

    for write in &journal.writes {
        let bytes = store
            .read_bounded(&write.stage, dbmd_core::parser::MAX_DBMD_FILE_BYTES)
            .map_err(|error| {
                CliError::runtime(format!(
                    "cannot read staged linker {}: {error}",
                    write.stage.display()
                ))
            })?;
        let current = read_optional(store, &write.target)?.ok_or_else(|| {
            rename_conflict(
                &write.target,
                "linker was removed after transaction preflight",
            )
        })?;
        if matches_fingerprint(&current, &write.after) {
            // A previous attempt already committed this exact linker.
        } else if matches_fingerprint(&current, &write.before) {
            store.write_atomic(&write.target, &bytes).map_err(|error| {
                CliError::runtime(format!(
                    "cannot commit linker {}: {error}",
                    write.target.display()
                ))
            })?;
        } else {
            return Err(rename_conflict(
                &write.target,
                "linker changed after transaction preflight",
            ));
        }
        step += 1;
        maybe_fail(step)?;
    }

    match read_optional(store, &journal.old)? {
        None => {}
        Some(current) if matches_fingerprint(&current, &journal.source_before) => {
            match store.remove_file(&journal.old) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CliError::runtime(format!(
                        "cannot remove old rename source: {error}"
                    )))
                }
            }
        }
        Some(_) => {
            return Err(rename_conflict(
                &journal.old,
                "source changed after transaction preflight",
            ))
        }
    }
    step += 1;
    maybe_fail(step)?;

    // Derived catalogs are rebuildable but are completed before clearing the
    // journal so a crash during refresh simply retries the idempotent projection.
    let mut index_warning = index_on_rename(store, &journal.old, &journal.new);
    for linker in &journal.rewritten_linkers {
        if let Some(warning) = index_on_write(store, linker) {
            index_warning.get_or_insert(warning);
        }
    }
    step += 1;
    maybe_fail(step)?;

    store
        .remove_file(Path::new(RENAME_JOURNAL))
        .map_err(|error| CliError::runtime(format!("cannot clear rename journal: {error}")))?;

    // Stages are unreachable after the journal clears. Best-effort deletion is
    // sufficient; a crash here leaves only hidden orphan files, never ambiguous
    // authored state.
    let _ = store.remove_file(&journal.source_stage);
    for write in &journal.writes {
        let _ = store.remove_file(&write.stage);
    }
    Ok(index_warning)
}

fn prepare_source_bytes(original: &[u8], old: &Path, new: &Path) -> (Vec<u8>, bool) {
    let rewritten = dbmd_core::graph::rewrite_links_to_bytes(original, old, new);
    let link_changed = rewritten != original;
    let Ok(rewritten_text) = std::str::from_utf8(&rewritten) else {
        return (rewritten, link_changed);
    };
    if let Ok(parsed) = dbmd_core::parser::split_frontmatter(rewritten_text, old) {
        if let Ok(mut frontmatter) =
            dbmd_core::parser::Frontmatter::parse(&parsed.frontmatter_yaml, old)
        {
            frontmatter.updated = Some(dbmd_core::now());
            return (
                dbmd_core::parser::render_file(&frontmatter, &parsed.body).into_bytes(),
                link_changed,
            );
        }
    }
    (rewritten, link_changed)
}

fn prepare_link_rewrite(
    store: &dbmd_core::Store,
    rel: &Path,
    old_rel: &Path,
    new_rel: &Path,
) -> Result<Option<LinkRewritePlan>, RewriteError> {
    let bytes = store
        .read_bounded(rel, dbmd_core::parser::MAX_DBMD_FILE_BYTES)
        .map_err(|error| {
            RewriteError::Io(CliError::runtime(format!(
                "cannot read linker {}: {error}",
                rel.display()
            )))
        })?;
    let rewritten = dbmd_core::graph::rewrite_links_to_bytes(&bytes, old_rel, new_rel);
    Ok((rewritten != bytes).then_some(LinkRewritePlan {
        before: bytes,
        after: rewritten,
    }))
}

#[derive(Debug)]
struct LinkRewritePlan {
    before: Vec<u8>,
    after: Vec<u8>,
}

/// Outcome of preparing a single linker rewrite.
#[derive(Debug)]
enum RewriteError {
    /// A genuine I/O failure (permissions, removed file, write error). Fatal to
    /// rename preparation, before the journal is claimed.
    Io(CliError),
}

/// Test-only wrapper for the path-based compatibility helper. Production
/// rename reads and writes exclusively through the retained [`Store`]
/// capability in [`prepare_link_rewrite`] and [`apply_rename_journal`].
///
/// Rewrite every `[[old]]` wiki-link in a file to `[[new]]`, delegating the
/// link grammar to [`dbmd_core::graph::rewrite_links_to`] — the write-side twin
/// of the core's backlink parser, so the rewrite recognizes exactly the edges
/// `Store::find_links_to` reported. Returns `Ok(true)` if the file changed,
/// `Ok(false)` for a no-op. Reads + writes the raw bytes (not the parser
/// round-trip) so a link inside frontmatter or body is rewritten uniformly and
/// nothing else is reflowed.
///
/// Invalid UTF-8 outside a link target is preserved byte-for-byte by
/// [`dbmd_core::graph::rewrite_links_to_bytes`], so externally dropped legacy
/// text cannot force a dangling skipped link. Read/write failures remain fatal.
#[cfg(test)]
fn rewrite_links_in_file(abs: &Path, old_rel: &Path, new_rel: &Path) -> Result<bool, RewriteError> {
    let bytes = dbmd_core::fsx::read_bounded_nofollow(abs, dbmd_core::parser::MAX_DBMD_FILE_BYTES)
        .map_err(|error| {
            RewriteError::Io(CliError::runtime(format!(
                "cannot read linker {}: {error}",
                abs.display()
            )))
        })?;
    let rewritten = dbmd_core::graph::rewrite_links_to_bytes(&bytes, old_rel, new_rel);
    if rewritten == bytes {
        return Ok(false);
    }
    dbmd_core::write_atomic(abs, &rewritten).map_err(|error| {
        RewriteError::Io(CliError::runtime(format!(
            "cannot finalize rewrite: {error}"
        )))
    })?;
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
        eprintln!("dbmd: warning: {}", sanitize_single_line(w));
    }
    if ctx.json {
        let out = serde_json::json!({
            "renamed": { "from": old, "to": new },
            "links_rewritten": rewritten,
        });
        println!("{out}");
    } else {
        let files = if rewritten == 1 { "file" } else { "files" };
        println!(
            "renamed {} -> {} ({rewritten} {files} rewritten)",
            sanitize_single_line(old),
            sanitize_single_line(new)
        );
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
        let original = "Only [[records/concepts/elsewhere]] here.";
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

    fn staged_journal(store: &dbmd_core::Store) -> RenameJournal {
        let transaction_id = "fault-injection".to_string();
        let root = PathBuf::from(".dbmd")
            .join("rename-transactions")
            .join(&transaction_id);
        let source_stage = root.join("source.stage");
        let linker_stage = root.join("linker-00000000.stage");
        let source_before = b"---\ntype: note\nsummary: Target\n---\nold target\n";
        let source_after = b"---\ntype: note\nsummary: Target\n---\nnew target\n";
        let linker_before = b"---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n";
        let linker_after = b"---\ntype: note\nsummary: Linker\n---\n[[records/notes/new]]\n";
        store.write_atomic_new(&source_stage, source_after).unwrap();
        store.write_atomic_new(&linker_stage, linker_after).unwrap();
        let journal = RenameJournal {
            version: 2,
            transaction_id,
            old: PathBuf::from("records/notes/old.md"),
            new: PathBuf::from("records/notes/new.md"),
            source_stage,
            source_before: fingerprint(source_before),
            source_after: fingerprint(source_after),
            writes: vec![JournalWrite {
                target: PathBuf::from("records/notes/linker.md"),
                stage: linker_stage,
                before: fingerprint(linker_before),
                after: fingerprint(linker_after),
            }],
            rewritten: 1,
            rewritten_linkers: vec![PathBuf::from("records/notes/linker.md")],
        };
        let encoded = serde_json::to_vec(&journal).unwrap();
        store
            .write_atomic_new(Path::new(RENAME_JOURNAL), &encoded)
            .unwrap();
        journal
    }

    #[test]
    fn rename_journal_recovers_after_every_commit_boundary() {
        for fail_after in 1..=4 {
            let dir = make_store();
            let notes = dir.path().join("records/notes");
            std::fs::create_dir_all(&notes).unwrap();
            std::fs::write(
                notes.join("old.md"),
                "---\ntype: note\nsummary: Target\n---\nold target\n",
            )
            .unwrap();
            std::fs::write(
                notes.join("linker.md"),
                "---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n",
            )
            .unwrap();
            let store = dbmd_core::Store::open_strict(dir.path()).unwrap();
            let journal = staged_journal(&store);

            let error = apply_rename_journal(&store, &journal, Some(fail_after))
                .expect_err("fault injection must interrupt the first attempt");
            assert!(error.message.contains("injected rename failure"));

            // At every intermediate point at least one valid target exists:
            // old is retained until destination + every linker are committed.
            assert!(
                notes.join("old.md").exists() || notes.join("new.md").exists(),
                "failure after step {fail_after} must never leave a dangling target"
            );

            let recovered = recover_pending_rename(&store)
                .expect("recovery succeeds")
                .expect("journal was pending");
            assert_eq!(recovered.old, Path::new("records/notes/old.md"));
            assert!(!notes.join("old.md").exists());
            assert_eq!(
                std::fs::read_to_string(notes.join("new.md")).unwrap(),
                "---\ntype: note\nsummary: Target\n---\nnew target\n"
            );
            assert!(std::fs::read_to_string(notes.join("linker.md"))
                .unwrap()
                .contains("[[records/notes/new]]"));
            assert!(!dir.path().join(RENAME_JOURNAL).exists());
        }
    }

    #[test]
    fn destination_race_never_clobbers_or_rewrites_linkers() {
        let dir = make_store();
        let notes = dir.path().join("records/notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("old.md"), "old").unwrap();
        std::fs::write(notes.join("linker.md"), "[[records/notes/old]]").unwrap();
        let store = dbmd_core::Store::open_strict(dir.path()).unwrap();
        let journal = staged_journal(&store);
        std::fs::write(notes.join("new.md"), "attacker won destination race").unwrap();

        let error = apply_rename_journal(&store, &journal, None)
            .expect_err("a different destination must fail closed");
        assert_eq!(error.code, "RENAME_CONFLICT");
        assert_eq!(
            std::fs::read_to_string(notes.join("old.md")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(notes.join("new.md")).unwrap(),
            "attacker won destination race"
        );
        assert_eq!(
            std::fs::read_to_string(notes.join("linker.md")).unwrap(),
            "[[records/notes/old]]"
        );
    }

    #[test]
    fn recovery_refuses_external_linker_edit_without_any_mutation() {
        let dir = make_store();
        let notes = dir.path().join("records/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let old = "---\ntype: note\nsummary: Target\n---\nold target\n";
        let externally_edited =
            "---\ntype: note\nsummary: Linker\n---\nexternal edit must survive\n";
        std::fs::write(notes.join("old.md"), old).unwrap();
        std::fs::write(
            notes.join("linker.md"),
            "---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n",
        )
        .unwrap();
        let store = dbmd_core::Store::open_strict(dir.path()).unwrap();
        let journal = staged_journal(&store);
        std::fs::write(notes.join("linker.md"), externally_edited).unwrap();

        let error = apply_rename_journal(&store, &journal, None)
            .expect_err("a third linker state must fail closed");
        assert_eq!(error.code, "RENAME_CONFLICT");
        assert_eq!(std::fs::read_to_string(notes.join("old.md")).unwrap(), old);
        assert!(
            !notes.join("new.md").exists(),
            "preflight must reject before installing the destination"
        );
        assert_eq!(
            std::fs::read_to_string(notes.join("linker.md")).unwrap(),
            externally_edited
        );
        assert!(
            dir.path().join(RENAME_JOURNAL).exists(),
            "the journal remains for explicit operator recovery"
        );
    }

    #[test]
    fn recovery_refuses_external_source_edit_without_any_mutation() {
        let dir = make_store();
        let notes = dir.path().join("records/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let external = "---\ntype: note\nsummary: Target\n---\nexternal source edit\n";
        let linker = "---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n";
        std::fs::write(
            notes.join("old.md"),
            "---\ntype: note\nsummary: Target\n---\nold target\n",
        )
        .unwrap();
        std::fs::write(notes.join("linker.md"), linker).unwrap();
        let store = dbmd_core::Store::open_strict(dir.path()).unwrap();
        let journal = staged_journal(&store);
        std::fs::write(notes.join("old.md"), external).unwrap();

        let error = apply_rename_journal(&store, &journal, None)
            .expect_err("a third source state must fail closed");
        assert_eq!(error.code, "RENAME_CONFLICT");
        assert_eq!(
            std::fs::read_to_string(notes.join("old.md")).unwrap(),
            external
        );
        assert!(!notes.join("new.md").exists());
        assert_eq!(
            std::fs::read_to_string(notes.join("linker.md")).unwrap(),
            linker
        );
    }

    #[test]
    fn recovery_refuses_corrupt_stage_without_any_mutation() {
        let dir = make_store();
        let notes = dir.path().join("records/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let old = "---\ntype: note\nsummary: Target\n---\nold target\n";
        let linker = "---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n";
        std::fs::write(notes.join("old.md"), old).unwrap();
        std::fs::write(notes.join("linker.md"), linker).unwrap();
        let store = dbmd_core::Store::open_strict(dir.path()).unwrap();
        let journal = staged_journal(&store);
        store
            .write_atomic(&journal.writes[0].stage, b"corrupt stage")
            .unwrap();

        let error = apply_rename_journal(&store, &journal, None)
            .expect_err("stage digest mismatch must fail closed");
        assert_eq!(error.code, "RENAME_CONFLICT");
        assert_eq!(std::fs::read_to_string(notes.join("old.md")).unwrap(), old);
        assert!(!notes.join("new.md").exists());
        assert_eq!(
            std::fs::read_to_string(notes.join("linker.md")).unwrap(),
            linker
        );
    }

    #[cfg(unix)]
    #[test]
    fn rename_recovery_stays_on_opened_root_after_path_swap() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("store");
        let notes = root.join("records/notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            root.join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: T\n---\n",
        )
        .unwrap();
        std::fs::write(
            notes.join("old.md"),
            "---\ntype: note\nsummary: Target\n---\nold target\n",
        )
        .unwrap();
        std::fs::write(
            notes.join("linker.md"),
            "---\ntype: note\nsummary: Linker\n---\n[[records/notes/old]]\n",
        )
        .unwrap();
        let store = dbmd_core::Store::open_strict(&root).unwrap();
        let journal = staged_journal(&store);

        let detached = sandbox.path().join("detached");
        std::fs::rename(&root, &detached).unwrap();
        let replacement = sandbox.path().join("replacement");
        std::fs::create_dir_all(replacement.join("records/notes")).unwrap();
        std::fs::write(replacement.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        std::fs::write(
            replacement.join("records/notes/old.md"),
            "replacement sentinel",
        )
        .unwrap();
        std::fs::write(
            replacement.join("records/notes/linker.md"),
            "replacement linker sentinel",
        )
        .unwrap();
        symlink(&replacement, &root).unwrap();

        apply_rename_journal(&store, &journal, None).unwrap();
        assert!(!detached.join("records/notes/old.md").exists());
        assert!(detached.join("records/notes/new.md").exists());
        assert!(
            std::fs::read_to_string(detached.join("records/notes/linker.md"))
                .unwrap()
                .contains("[[records/notes/new]]")
        );
        assert_eq!(
            std::fs::read_to_string(replacement.join("records/notes/old.md")).unwrap(),
            "replacement sentinel"
        );
        assert_eq!(
            std::fs::read_to_string(replacement.join("records/notes/linker.md")).unwrap(),
            "replacement linker sentinel"
        );
    }
}
