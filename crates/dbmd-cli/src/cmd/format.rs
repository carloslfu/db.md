//! `dbmd format <file>` — canonical re-emit, writes back in place.
//!
//! Thin wrapper: open the store the file lives in, refuse the write if the file
//! is a `DB.md ## Policies → ### Frozen pages` path (exit `4`), read it via the
//! `dbmd_core::parser` read path, and re-emit it via the parser write path
//! (canonical frontmatter key order + verbatim body, atomic temp-rename). The
//! frontmatter ordering and the atomic write are entirely `dbmd-core`'s; this
//! body resolves the store, applies the policy gate, and reports the result.

use std::path::{Path, PathBuf};

use dbmd_core::parser::{read_file, write_file};
use dbmd_core::validate::codes;
use dbmd_core::Store;

use crate::cli::FormatArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd format`.
pub fn run(ctx: &Context, args: &FormatArgs) -> CliResult {
    let file = Path::new(&args.file);

    // Resolve the store the file lives in so the frozen-page policy can be
    // consulted. `dbmd format` takes only a file path (no `--dir`); the store
    // root is the nearest ancestor that carries a `DB.md` marker.
    let store = locate_store(file)?;
    let rel = store_relative(&store, file);

    // Policy gate: a frozen page is never rewritten, even by a no-op reformat.
    // The same canonical `.md`-insensitive matcher every write surface uses.
    if store.config.is_frozen(&rel) {
        return Err(CliError::new(
            ExitCode::Policy,
            codes::POLICY_FROZEN_PAGE,
            format!("`{}` is a frozen page; refusing to format", rel.display()),
        )
        .with_hint("remove it from DB.md ## Policies → ### Frozen pages to allow writes"));
    }

    // Read (frontmatter + verbatim body), then re-emit canonically. The writer
    // preserves the body byte-for-byte and only normalizes the frontmatter
    // block's key order / YAML style.
    let original = std::fs::read_to_string(file).map_err(CliError::from)?;
    let (frontmatter, body) = read_file(file).map_err(|e| {
        CliError::from(dbmd_core::Error::from(e))
            .with_hint(format!("could not read `{}`", args.file))
    })?;
    write_file(file, &frontmatter, &body).map_err(|e| {
        CliError::from(dbmd_core::Error::from(e))
            .with_hint(format!("could not write `{}`", args.file))
    })?;

    // Report whether the canonical form differed from what was on disk —
    // computed in-memory from the same pieces the writer just emitted, NOT a
    // re-read. The atomic write already succeeded, so a transient re-read
    // failure (or a concurrent delete) must not turn a successful format into
    // an error exit; reconstructing the bytes also avoids re-reading what we
    // just wrote. This mirrors `parser::write_file`'s composition exactly.
    let canonical = format!("---\n{}---\n{}", frontmatter.to_yaml(), body);
    let changed = canonical != original;

    if ctx.json {
        let obj = serde_json::json!({
            "file": rel.to_string_lossy(),
            "changed": changed,
        });
        let mut s = serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string());
        s.push('\n');
        print!("{s}");
    } else if changed {
        println!("formatted {}", rel.display());
    } else {
        println!("{} already canonical", rel.display());
    }
    Ok(())
}

/// Find the nearest db.md store owning the file. A descendant `DB.md` starts a
/// separate store boundary, so an outer store's policy must not govern writes
/// beneath it. A file outside any store is the stable `NOT_A_STORE` error.
fn locate_store(file: &Path) -> Result<Store, CliError> {
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
fn store_relative(store: &Store, file: &Path) -> PathBuf {
    let canonical_file = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let canonical_root = std::fs::canonicalize(&store.root).unwrap_or_else(|_| store.root.clone());
    canonical_file
        .strip_prefix(&canonical_root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nested DB.md starts a distinct store. Formatting a file below it must
    /// load the nested policy and compute a nested-store-relative path.
    #[test]
    fn locate_store_stops_at_nearest_store_boundary() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: T\n---\n# Outer store\n",
        )
        .unwrap();

        let docs = root.join("sources").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // The frozen page itself.
        std::fs::write(
            docs.join("contract.md"),
            "---\ntype: pdf-source\nsummary: A frozen contract page\n---\n# Contract\n",
        )
        .unwrap();
        // The nested store owns the contract and freezes its local path.
        std::fs::write(
            docs.join("DB.md"),
            "---\ntype: db-md\nscope: research\nowner: Nested\n---\n\n# Nested\n\n## Policies\n\n### Frozen pages\n- contract.md\n",
        )
        .unwrap();

        let contract = docs.join("contract.md");
        let store = locate_store(&contract).expect("store must resolve");

        assert_eq!(
            std::fs::canonicalize(&store.root).unwrap(),
            std::fs::canonicalize(&docs).unwrap(),
            "the nearest nested store must own the file"
        );

        let rel = store_relative(&store, &contract);
        assert_eq!(rel, PathBuf::from("contract.md"));
        assert!(
            store.config.is_frozen(&rel),
            "the nested store's frozen-page policy must apply"
        );
    }
}
