// SPDX-License-Identifier: Apache-2.0

//! `dbmd show` — the single-file structured record.
//!
//! The core contract pinned here: `show --json` is the random-access form of
//! `emit` — its object equals the file's `emit --ndjson` line for every file
//! in the canonical corpus. Read paths run against corpus-a; the exact-text
//! case runs against a scratch store with pinned timestamps.

mod common;

use common::{corpus_a, dbmd, write_file};

/// `show --json` must equal the `emit --ndjson` entry for the same file —
/// checked for EVERY corpus-a file, so the two projections cannot drift.
#[test]
fn show_json_equals_emit_ndjson_for_every_corpus_file() {
    let assert = dbmd()
        .args(["--json", "emit", "--ndjson"])
        .arg(corpus_a())
        .assert()
        .success();
    let ndjson = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let mut checked = 0usize;
    for line in ndjson.lines() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        let path = entry["path"].as_str().unwrap().to_string();

        let assert = dbmd()
            .args(["--json", "show"])
            .arg(corpus_a().join(&path))
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let shown: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(shown, entry, "show/{path} drifted from its emit entry");
        checked += 1;
    }
    assert!(
        checked > 10,
        "corpus-a should exercise many files ({checked})"
    );
}

/// The root `DB.md` is showable: no layer, `type: db-md`.
#[test]
fn show_handles_db_md() {
    let assert = dbmd()
        .args(["--json", "show"])
        .arg(corpus_a().join("DB.md"))
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let shown: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(shown["path"], serde_json::json!("DB.md"));
    assert_eq!(shown["layer"], serde_json::Value::Null);
    assert_eq!(shown["type"], serde_json::json!("db-md"));
}

/// Exact text form: derived header fields, blank line, verbatim body.
#[test]
fn show_text_exact() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path();
    write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n# T\n",
    );
    let contents = "---\ntype: note\nsummary: A tiny note\ncreated: 2026-01-02T03:04:05Z\nupdated: 2026-01-03T04:05:06Z\n---\n# Note title\n\nHello [[records/notes/b]] world.\n";
    let file = write_file(store, "records/notes/a.md", contents);

    let digest = ring::digest::digest(&ring::digest::SHA256, contents.as_bytes());
    let sha256: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();

    let assert = dbmd().arg("show").arg(&file).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let expected = format!(
        "path: records/notes/a.md\n\
         layer: record\n\
         type: note\n\
         meta-type: fact\n\
         title: Note title\n\
         summary: A tiny note\n\
         created: 2026-01-02T03:04:05+00:00\n\
         updated: 2026-01-03T04:05:06+00:00\n\
         links: records/notes/b.md\n\
         sha256: {sha256}\n\
         \n\
         # Note title\n\
         \n\
         Hello [[records/notes/b]] world.\n"
    );
    assert_eq!(stdout, expected);
}

/// A missing file is a runtime error (exit 1), the store still resolving.
#[test]
fn show_missing_file_fails() {
    dbmd()
        .arg("show")
        .arg(corpus_a().join("records/contacts/no-such-file.md"))
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime
}

/// Outside any store the stable NOT_A_STORE contract applies (exit 3).
#[test]
fn show_outside_store_is_not_a_store() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = write_file(dir.path(), "orphan.md", "---\ntype: note\n---\nx\n");
    dbmd().arg("show").arg(&file).assert().failure().code(3); // ExitCode::NotAStore
}
