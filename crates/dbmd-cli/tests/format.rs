//! Integration tests for `dbmd format <file>`.
//!
//! `format` re-emits a file's frontmatter in canonical key order (preserving
//! the body verbatim) and writes back atomically, refusing frozen pages. These
//! tests always operate on synthetic temp stores (or a temp *copy* of a
//! corpus), never the committed fixtures, because `format` writes in place.

mod common;

use common::{copy_store_to_temp, corpus_a, dbmd, write_db_md, write_file};

/// The canonical key order `dbmd-core` emits: type, id, created, updated,
/// summary, then type-specific (sorted), then status, tags. We build a file
/// with the universal keys deliberately scrambled and assert the rewrite
/// reorders them.
const SCRAMBLED: &str = "\
---
summary: a contact
tags:
  - vip
status: active
updated: 2026-05-02T00:00:00+00:00
created: 2026-05-01T00:00:00+00:00
type: contact
email: sarah@acme.com
---

# Sarah

Body stays [[records/companies/acme]] verbatim.
";

#[test]
fn reorders_frontmatter_to_canonical_and_reports_changed() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    let file = write_file(tmp.path(), "records/contacts/sarah.md", SCRAMBLED);

    let out = dbmd().arg("format").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("formatted records/contacts/sarah.md"),
        "text mode reports the store-relative formatted path: {stdout:?}"
    );

    let rewritten = std::fs::read_to_string(&file).unwrap();
    // Universal head keys appear in canonical order, before the type-specific
    // `email`, with `status`/`tags` in the tail.
    let order: Vec<&str> = [
        "type", "created", "updated", "summary", "email", "status", "tags",
    ]
    .into_iter()
    .collect();
    let positions: Vec<usize> = order
        .iter()
        .map(|k| {
            rewritten
                .find(&format!("{k}:"))
                .unwrap_or_else(|| panic!("`{k}:` present after format:\n{rewritten}"))
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert_eq!(
        positions, sorted,
        "keys are in canonical order:\n{rewritten}"
    );

    // The body is preserved verbatim, including the wiki-link.
    assert!(rewritten.contains("Body stays [[records/companies/acme]] verbatim."));
    // The file still opens and closes with `---` fences.
    assert!(rewritten.starts_with("---\n"));
    assert!(rewritten.contains("\n---\n"));
}

#[test]
fn already_canonical_file_reports_unchanged() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    // First normalize once.
    let file = write_file(tmp.path(), "records/contacts/sarah.md", SCRAMBLED);
    dbmd().arg("format").arg(&file).assert().success();
    let canonical = std::fs::read_to_string(&file).unwrap();

    // Second format is a no-op: changed == false, bytes identical.
    let out = dbmd()
        .arg("--json")
        .arg("format")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["file"], "records/contacts/sarah.md");
    assert_eq!(parsed["changed"], false, "idempotent on a canonical file");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        canonical,
        "a no-op format leaves the bytes untouched"
    );
}

#[test]
fn format_is_idempotent() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    let file = write_file(tmp.path(), "records/contacts/sarah.md", SCRAMBLED);

    dbmd().arg("format").arg(&file).assert().success();
    let once = std::fs::read_to_string(&file).unwrap();
    dbmd().arg("format").arg(&file).assert().success();
    let twice = std::fs::read_to_string(&file).unwrap();
    assert_eq!(once, twice, "format(format(x)) == format(x)");
}

#[test]
fn frozen_page_is_refused_exit_four_no_write() {
    let tmp = tempfile::TempDir::new().unwrap();
    // DB.md declaring a frozen page.
    std::fs::write(
        tmp.path().join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: T\n---\n\n# S\n\n\
         ## Policies\n\n### Frozen pages\n- `records/synthesis/plan.md` — signed off.\n",
    )
    .unwrap();
    let file = write_file(
        tmp.path(),
        "records/synthesis/plan.md",
        SCRAMBLED, // deliberately non-canonical, to prove no rewrite happens
    );
    let before = std::fs::read_to_string(&file).unwrap();

    dbmd().arg("format").arg(&file).assert().failure().code(4); // ExitCode::Policy

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "a refused format must not modify the frozen file"
    );
}

#[test]
fn corpus_a_copy_format_preserves_body_and_is_idempotent() {
    // Format a COPY of a committed fixture (never the committed file). We do not
    // assume the corpus's hand-authored YAML style equals `dbmd-core`'s emitter
    // byte-for-byte — only the invariants that MUST hold: the markdown body is
    // preserved verbatim, and a second format is a no-op (idempotent).
    let (_guard, store) = copy_store_to_temp(&corpus_a());
    let file = store.join("records/contacts/sarah-chen.md");
    let original = std::fs::read_to_string(&file).unwrap();
    // The body after the closing fence — must survive verbatim.
    let original_body = original.rsplit_once("\n---\n").map(|(_, b)| b.to_string());

    dbmd().arg("format").arg(&file).assert().success();
    let once = std::fs::read_to_string(&file).unwrap();
    if let Some(body) = &original_body {
        let after_body = once.rsplit_once("\n---\n").map(|(_, b)| b.to_string());
        assert_eq!(
            after_body.as_ref(),
            Some(body),
            "the markdown body is preserved verbatim across a reformat"
        );
    }

    // Idempotence: format(format(x)) == format(x).
    let out = dbmd()
        .arg("--json")
        .arg("format")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed["changed"], false,
        "the second format of an already-canonical file is a no-op"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), once);
}

#[test]
fn missing_file_is_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    let missing = tmp.path().join("records/contacts/ghost.md");
    // A nonexistent file under a real store: the read fails (runtime error).
    dbmd().arg("format").arg(&missing).assert().failure();
}
