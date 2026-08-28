// SPDX-License-Identifier: Apache-2.0

//! `dbmd body set|append` — whole-body edits.
//!
//! Writes run against scratch stores; `DBMD_NOW` pins the clock so the
//! `updated` re-stamp is byte-checkable. Assertions pin the body bytes, the
//! frontmatter contract (`updated` bumped, `summary` untouched), and the
//! stable error codes.

mod common;

use common::{dbmd, split_frontmatter_body, write_file};

const RECORD: &str = "---\ntype: widget\nsummary: Widget A\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\nalpha line\n";

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

/// `set` stores the content verbatim, re-stamps `updated`, leaves `summary`
/// alone, and folds the file into the write-through index.
#[test]
fn body_set_replaces_verbatim_and_restamps_updated() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    let assert = dbmd()
        .env("DBMD_NOW", "2026-05-05T05:05:05Z")
        .args(["--json", "body", "set"])
        .arg(&file)
        .args(["--text", "new body\nwith two lines"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(out["file"], serde_json::json!("records/widgets/a.md"));
    assert_eq!(out["action"], serde_json::json!("set"));
    assert_eq!(out["index_updated"], serde_json::json!(true));

    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(body, "new body\nwith two lines"); // verbatim — no forced newline

    for (key, expected) in [
        ("updated", "2026-05-05T05:05:05+00:00"),
        ("created", "2026-01-01T00:00:00+00:00"),
        ("summary", "Widget A"),
    ] {
        let assert = dbmd()
            .args(["fm", "get"])
            .arg(&file)
            .arg(key)
            .assert()
            .success();
        let value = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert_eq!(value.trim_end(), expected, "{key}");
    }
}

/// `set --text ""` empties the body (a frontmatter-only record, the TodoMVC
/// shape); omitting both content flags is a clap usage error.
#[test]
fn body_set_empty_and_missing_content() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    dbmd()
        .args(["body", "set"])
        .arg(&file)
        .args(["--text", ""])
        .assert()
        .success();
    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(body, "");

    dbmd()
        .args(["body", "set"])
        .arg(&file)
        .assert()
        .failure()
        .code(2); // clap usage: one of --text / --body-file is required
}

/// `append` joins raw content: the joint gains a newline only when the
/// existing body lacks one, and the content rides verbatim.
#[test]
fn body_append_joins_raw() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    dbmd()
        .args(["body", "append"])
        .arg(&file)
        .args(["--text", "beta line"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(body, "alpha line\nbeta line");
}

/// `--body-file -` reads standard input.
#[test]
fn body_set_reads_stdin() {
    let (_tmp, store) = scratch_store();
    let file = store.join("records/widgets/a.md");

    dbmd()
        .args(["body", "set"])
        .arg(&file)
        .args(["--body-file", "-"])
        .write_stdin("from stdin\n")
        .assert()
        .success();
    let text = std::fs::read_to_string(&file).unwrap();
    let (_, body) = split_frontmatter_body(&text).unwrap();
    assert_eq!(body, "from stdin\n");
}

/// A frozen page is never body-edited (exit 4, `POLICY_FROZEN_PAGE`).
#[test]
fn body_edit_frozen_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path();
    write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Policies\n\n### Frozen pages\n- `records/widgets/a.md` — signed off.\n",
    );
    write_file(store, "records/widgets/a.md", RECORD);

    let assert = dbmd()
        .args(["--json", "body", "set"])
        .arg(store.join("records/widgets/a.md"))
        .args(["--text", "nope"])
        .assert()
        .failure()
        .code(4); // ExitCode::Policy
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(
        err["error"]["code"],
        serde_json::json!("POLICY_FROZEN_PAGE")
    );

    let text = std::fs::read_to_string(store.join("records/widgets/a.md")).unwrap();
    assert!(text.contains("alpha line"), "refusal must not write");
}

/// Outside any store the stable NOT_A_STORE contract applies (exit 3).
#[test]
fn body_edit_outside_store_is_not_a_store() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = write_file(dir.path(), "orphan.md", RECORD);
    dbmd()
        .args(["body", "set"])
        .arg(&file)
        .args(["--text", "x"])
        .assert()
        .failure()
        .code(3); // ExitCode::NotAStore
}
