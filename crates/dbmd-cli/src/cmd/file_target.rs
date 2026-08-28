// SPDX-License-Identifier: Apache-2.0

//! File-scoped store resolution — the shared plumbing for verbs that take a
//! bare `FILE` argument and no `--dir` (`format`, `show`, `body`, `section`).
//!
//! The store root is the nearest ancestor of the file that carries a `DB.md`
//! marker; a descendant `DB.md` starts a separate store boundary, so an outer
//! store's policy must never govern writes beneath it. Extracted from
//! `cmd/format.rs` when `show` and the body/section editors joined it as
//! consumers.

use std::path::{Path, PathBuf};

use dbmd_core::Store;

use crate::error::CliError;

/// Resolve the file argument to `<canonical parent>/<leaf>` before any store
/// open, so the ancestor walk works for a bare relative arg too.
pub(crate) fn lexical_absolute_before_open(file: &Path) -> Result<PathBuf, CliError> {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent).map_err(CliError::from)?;
    let leaf = file.file_name().ok_or_else(|| {
        CliError::runtime(format!("file path `{}` has no filename", file.display()))
    })?;
    Ok(parent.join(leaf))
}

/// Find the nearest db.md store owning the file. A descendant `DB.md` starts a
/// separate store boundary, so an outer store's policy must not govern writes
/// beneath it. A file outside any store is the stable `NOT_A_STORE` error.
pub(crate) fn locate_store(file: &Path) -> Result<Store, CliError> {
    let start = file.parent().unwrap_or(Path::new("."));
    // Canonicalize so the walk-up works for a bare relative `file` arg too; fall
    // back to the literal path if canonicalization fails (e.g. file absent — the
    // read below then surfaces the real I/O error).
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    match start.ancestors().find(|path| Store::is_db_md_store(path)) {
        Some(root) => Store::open_strict(root).map_err(CliError::from),
        // No ancestor is a store: surface NOT_A_STORE against the file's directory.
        None => Store::open_strict(&start).map_err(CliError::from),
    }
}

/// The file's store-relative path. Canonicalizes the file and strips the store
/// root; if that fails (file absent), falls back to the literal arg so the
/// frozen-page comparison still has something to match.
pub(crate) fn store_relative(store: &Store, file: &Path) -> PathBuf {
    store
        .capability_relative(file)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| file.to_path_buf())
}
