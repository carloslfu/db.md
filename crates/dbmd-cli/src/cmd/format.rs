//! `dbmd format <file>` — canonical re-emit, writes back in place.
//!
//! Thin wrapper: open the store the file lives in, refuse the write if the file
//! is a `DB.md ## Policies → ### Frozen pages` path (exit `4`), read it via the
//! `dbmd_core::parser` read path, and re-emit it via the parser write path
//! (canonical frontmatter key order + verbatim body, atomic temp-rename). The
//! frontmatter ordering and the atomic write are entirely `dbmd-core`'s; this
//! body resolves the store, applies the policy gate, and reports the result.

use std::path::Path;

use dbmd_core::parser::{split_frontmatter, Frontmatter, MAX_DBMD_FILE_BYTES};
use dbmd_core::validate::codes;

use crate::cli::FormatArgs;
use crate::cmd::file_target::{lexical_absolute_before_open, locate_store, store_relative};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd format`.
pub fn run(ctx: &Context, args: &FormatArgs) -> CliResult {
    let input = Path::new(&args.file);
    let file = lexical_absolute_before_open(input)?;

    // Resolve the store the file lives in so the frozen-page policy can be
    // consulted. `dbmd format` takes only a file path (no `--dir`); the store
    // root is the nearest ancestor that carries a `DB.md` marker.
    let store = locate_store(&file)?;
    let _transaction = store.transaction().map_err(CliError::from)?;
    let rel = store_relative(&store, &file);

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
    let original = store
        .read_bounded(&rel, MAX_DBMD_FILE_BYTES)
        .map_err(CliError::from)
        .and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| CliError::runtime(format!("file is not UTF-8: {error}")))
        })?;
    let parsed = split_frontmatter(&original, &rel).map_err(|e| {
        CliError::from(dbmd_core::Error::from(e))
            .with_hint(format!("could not read `{}`", args.file))
    })?;
    let frontmatter = Frontmatter::parse(&parsed.frontmatter_yaml, &rel).map_err(|e| {
        CliError::from(dbmd_core::Error::from(e))
            .with_hint(format!("could not read `{}`", args.file))
    })?;
    let body = parsed.body;
    store.write_file(&rel, &frontmatter, &body).map_err(|e| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[cfg(unix)]
    #[test]
    fn format_refuses_external_symlink_before_reading_or_replacing_it() {
        use std::os::unix::fs::symlink;

        let store_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            store_dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: test\nowner: Test\n---\n# Test\n",
        )
        .unwrap();
        std::fs::create_dir_all(store_dir.path().join("records/notes")).unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(
            &secret,
            "---\ntype: note\nsummary: outside\n---\nTOP SECRET\n",
        )
        .unwrap();
        let link = store_dir.path().join("records/notes/leak.md");
        symlink(&secret, &link).unwrap();

        let store = locate_store(&link).unwrap();
        assert!(
            dbmd_core::store::ensure_path_within_store(&store.root, &link).is_err(),
            "an external symlink must fail before format reads its bytes"
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&secret)
            .unwrap()
            .contains("TOP SECRET"));
    }
}
