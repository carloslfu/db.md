//! Shared helpers for the `dbmd` CLI integration tests.
//!
//! These tests drive the real `dbmd` binary with `assert_cmd` against the
//! committed corpora (`tests/corpora/corpus-a-canonical` = happy path,
//! `corpus-b-edges` = designed-to-fail) and against synthetic temp stores when
//! a test needs byte-exact control or must write (so the committed corpus is
//! never mutated).
//!
//! Intent-derived, per the corpora's `EXPECTED/README.md`: assertions check the
//! properties that MUST hold (exit codes, the set of issue codes, which paths a
//! query/links call returns) rather than copying the tool's own emitted prose.

#![allow(dead_code)]

use std::path::PathBuf;

use assert_cmd::Command;

/// The repo-root `tests/corpora` directory, resolved from this crate's
/// manifest (`crates/dbmd-cli` → `../../tests/corpora`). Committed, read-only
/// fixtures — copy into a tempdir before any test that writes.
pub fn corpora_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpora")
}

/// Absolute path to the canonical happy-path store (`corpus-a-canonical`).
pub fn corpus_a() -> PathBuf {
    corpora_dir().join("corpus-a-canonical")
}

/// Absolute path to the designed-to-fail store (`corpus-b-edges`).
pub fn corpus_b() -> PathBuf {
    corpora_dir().join("corpus-b-edges")
}

/// Absolute path to a committed golden under `corpus-a-canonical/EXPECTED/`
/// (e.g. `validate.json`, `search.json`, `index/records/contacts/index.md`).
/// These goldens are intent-derived contracts the E2E suite asserts against.
pub fn corpus_a_expected(rel: &str) -> PathBuf {
    corpus_a().join("EXPECTED").join(rel)
}

/// Absolute path to a committed golden under `corpus-b-edges/EXPECTED/`
/// (e.g. `validate.json`, `not-a-store.json`, `policy-refusal/write.json`).
/// The designed-to-fail store's intent-derived contract — every value here is
/// hand-derived from `SPEC.md § Validation`, never copied from tool output.
pub fn corpus_b_expected(rel: &str) -> PathBuf {
    corpus_b().join("EXPECTED").join(rel)
}

/// Split a db.md file's text into `(frontmatter_block, body)` at the closing
/// `---` fence. `frontmatter_block` is the YAML between the fences (no fences);
/// `body` is everything after the closing fence's newline — the operator-edited
/// region a write must preserve byte-for-byte. Returns `None` when the text does
/// not open with a `---` fence. (Mirrors the parser's split so a round-trip test
/// can assert body preservation without depending on `dbmd-core` internals.)
pub fn split_frontmatter_body(text: &str) -> Option<(&str, &str)> {
    let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = stripped
        .strip_prefix("---\n")
        .or_else(|| stripped.strip_prefix("---\r\n"))?;
    let mut idx = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let fm = &rest[..idx];
            let body = &rest[idx + line.len()..];
            return Some((fm, body));
        }
        idx += line.len();
    }
    None
}

/// A fresh `dbmd` command (the binary built by this crate, located by
/// `assert_cmd`). Each test composes its own args/flags on top.
pub fn dbmd() -> Command {
    Command::cargo_bin("dbmd").expect("the `dbmd` binary builds for integration tests")
}

/// Recursively copy a committed corpus into a fresh tempdir and return the
/// `(tempdir guard, store root)`. The guard must be kept alive for the store to
/// exist; dropping it deletes the copy. Use for any test that writes (so the
/// committed fixture is never mutated).
pub fn copy_store_to_temp(src: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().expect("create tempdir");
    let dst = tmp.path().join("store");
    copy_dir_all(src, &dst);
    (tmp, dst)
}

/// Recursive directory copy (files + subdirs), skipping nothing — the corpus is
/// small. Used by [`copy_store_to_temp`].
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).expect("create dest dir");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let file_type = entry.file_type().expect("file type");
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Write a minimal but valid `DB.md` marker into `dir`, making it a db.md store.
pub fn write_db_md(dir: &std::path::Path) {
    std::fs::write(
        dir.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Test\n---\n\n# Test store\n",
    )
    .expect("write DB.md");
}

/// Write a file at `dir/<rel>`, creating parent directories. Returns the
/// absolute path written.
pub fn write_file(dir: &std::path::Path, rel: &str, contents: &str) -> PathBuf {
    let abs = dir.join(rel);
    std::fs::create_dir_all(abs.parent().expect("path has a parent")).expect("create parents");
    std::fs::write(&abs, contents).expect("write file");
    abs
}
