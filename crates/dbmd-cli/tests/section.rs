// SPDX-License-Identifier: Apache-2.0

//! `dbmd section get|set|append` — section-addressed reads and writes.
//!
//! `get` is store-free (any markdown file, file-relative lines); the edits
//! run against scratch stores through the shared body/section write surface.
//! Assertions pin the exact body bytes after each edit, the `updated`
//! re-stamp, and the stable `SECTION_NOT_FOUND` / `SECTION_AMBIGUOUS`
//! contracts.

mod common;

use common::{dbmd, split_frontmatter_body, write_file};

const RECORD: &str = "---\ntype: widget\nsummary: Widget A\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\nintro\n\n## Status\nactive\n\n### Sub-note\nnested\n\n## Log\n- one\n";

fn scratch_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path().to_path_buf();
    write_file(
        &store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n# T\n",
    );
    write_file(&store, "records/widgets/a.md", RECORD);
    (dir, store)
}

/// `get` prints the addressed section verbatim — heading line, content, and
/// deeper sub-sections — with file-relative lines under `--json` (the
/// `sections` frame: the 6-line frontmatter block offsets the body).
#[test]
fn section_get_prints_verbatim() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    let assert = dbmd()
        .args(["section", "get"])
        .arg(&file)
        .arg("Status")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "## Status\nactive\n\n### Sub-note\nnested\n\n");

    let assert = dbmd()
        .args(["--json", "section", "get"])
        .arg(&file)
        .arg("Sub-note")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["heading"], serde_json::json!("Sub-note"));
    assert_eq!(parsed["level"], serde_json::json!(3));
    assert_eq!(parsed["line"], serde_json::json!(12)); // 6 frontmatter lines + body line 6
    assert_eq!(
        parsed["body"],
        serde_json::json!("### Sub-note\nnested\n\n")
    );
}

/// A missing heading and a duplicated heading each fail with their stable
/// code, for `get` and for the edits alike.
#[test]
fn section_missing_and_ambiguous_contracts() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    let assert = dbmd()
        .args(["--json", "section", "get"])
        .arg(&file)
        .arg("Nope")
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("SECTION_NOT_FOUND"));

    let dup = write_file(
        &store,
        "records/widgets/dup.md",
        "---\ntype: widget\nsummary: Dup\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n## Twice\na\n\n## Twice\nb\n",
    );
    let assert = dbmd()
        .args(["--json", "section", "set"])
        .arg(&dup)
        .arg("Twice")
        .args(["--text", "x"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("SECTION_AMBIGUOUS"));
    assert_eq!(err["error"]["details"]["lines"], serde_json::json!([1, 4]));
}

/// `set` replaces the section's whole subtree, keeps the heading line, and
/// re-stamps `updated`; `summary` never moves.
#[test]
fn section_set_replaces_subtree() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    let assert = dbmd()
        .env("DBMD_NOW", "2026-05-05T05:05:05Z")
        .args(["--json", "section", "set"])
        .arg(&file)
        .arg("Status")
        .args(["--text", "paused"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(out["action"], serde_json::json!("set"));
    assert_eq!(out["created"], serde_json::json!(false));
    assert_eq!(out["level"], serde_json::json!(2));

    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(body, "intro\n\n## Status\npaused\n## Log\n- one\n");
    assert!(text.contains("updated: 2026-05-05T05:05:05+00:00"));
    assert!(text.contains("summary: Widget A"));
}

/// `append` lands at the end of the addressed section, before the next
/// sibling heading.
#[test]
fn section_append_lands_before_next_sibling() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    dbmd()
        .args(["section", "append"])
        .arg(&file)
        .arg("Log")
        .args(["--text", "- two"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(
        body,
        "intro\n\n## Status\nactive\n\n### Sub-note\nnested\n\n## Log\n- one\n- two\n"
    );
}

/// A missing heading fails without `--create` and upserts with it — one
/// separating blank line, the requested `--level`.
#[test]
fn section_create_upserts_at_end() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    let assert = dbmd()
        .args(["--json", "section", "set"])
        .arg(&file)
        .arg("Notes")
        .args(["--text", "fresh"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("SECTION_NOT_FOUND"));

    let assert = dbmd()
        .args(["--json", "section", "set"])
        .arg(&file)
        .arg("Notes")
        .args(["--text", "fresh", "--create", "--level", "3"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(out["created"], serde_json::json!(true));
    assert_eq!(out["level"], serde_json::json!(3));

    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert!(
        body.ends_with("## Log\n- one\n\n### Notes\nfresh\n"),
        "{body}"
    );
}

/// A frozen page is never section-edited (exit 4), and `get` still reads it
/// (reads are not writes).
#[test]
fn section_edit_frozen_refused_get_allowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path();
    write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Policies\n\n### Frozen pages\n- `records/widgets/a.md` — signed off.\n",
    );
    let file = write_file(store, "records/widgets/a.md", RECORD);

    dbmd()
        .args(["section", "set"])
        .arg(&file)
        .arg("Status")
        .args(["--text", "nope"])
        .assert()
        .failure()
        .code(4); // ExitCode::Policy

    dbmd()
        .args(["section", "get"])
        .arg(&file)
        .arg("Status")
        .assert()
        .success();
}
