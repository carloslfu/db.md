//! Regression tests for the September 2026 "unseen writes" failures. A real
//! store accumulated five schema errors and two warnings while every session's
//! `dbmd validate` reported zero issues:
//!
//!   - A script wrote `source-kind` / `original-path` for the schema's
//!     `source_kind` / `original_path`. `dbmd write` accepted the record, and
//!     the typo was reported only as a missing required field.
//!   - Two records declared assets that were never cataloged.
//!   - Two sources were written with summaries over 200 characters.
//!   - The default `dbmd validate` read its scope from `log.md`: only six
//!     standard kinds counted, so files logged under the store's declared kinds
//!     (or never logged) went unexamined whenever any standard entry existed.
//!   - The script crashed on `dbmd log … --dir`, `dbmd validate --dir` and an
//!     absolute `dbmd fm set` path run from outside the store.
//!
//! Each test reconstructs one failure through the real binary and asserts the
//! corrected behaviour, so none can silently return.

mod common;

use std::path::Path;

use common::{dbmd, write_file};

/// A store with the shape of the affected one: a schema with a required enum
/// field and an optional date, and store-declared log kinds.
fn store_with_schema(dir: &Path) {
    write_file(
        dir,
        "DB.md",
        "---\ntype: db-md\nscope: personal\nowner: Test\n---\n\n# Test store\n\n\
         Regression fixture.\n\n## Policies\n\n### Validation log kinds\n- audit\n- add\n\n\
         ## Schemas\n\n### artifact\n- title (required, string)\n\
         - source_kind (required, enum: pdf, asset, json)\n- captured_at (date)\n",
    );
}

fn artifact(summary: &str, extra: &str) -> String {
    format!(
        "---\ntype: artifact\nsummary: {summary}\ntitle: T\ncreated: 2026-09-22T19:11:11Z\n\
         updated: 2026-09-22T19:11:11Z\n{extra}---\n\n# T\n"
    )
}

/// `dbmd --json validate [--all] <dir>` → (exit code, issues array).
fn validate(dir: &Path, all: bool) -> (Option<i32>, Vec<serde_json::Value>) {
    let mut cmd = dbmd();
    cmd.arg("--json").arg("validate");
    if all {
        cmd.arg("--all");
    }
    let output = cmd.arg(dir).output().expect("run dbmd validate");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("validate --json prints JSON");
    (
        output.status.code(),
        json["issues"].as_array().cloned().unwrap_or_default(),
    )
}

/// Rebuild every index so a full `--all` sweep judges only the content under
/// test (files written directly here have no catalog entries yet).
fn rebuild_indexes(dir: &Path) {
    dbmd()
        .args(["index", "rebuild", "--dir", &dir.to_string_lossy()])
        .assert()
        .success();
}

fn has(issues: &[serde_json::Value], code: &str, file: &str) -> bool {
    issues
        .iter()
        .any(|issue| issue["code"] == code && issue["file"] == file)
}

fn body_file(dir: &Path) -> String {
    let path = dir.join("body.md");
    std::fs::write(&path, "# Body\n").unwrap();
    path.to_string_lossy().into_owned()
}

// ─────────────────────────────────────────────────────────────────────────────
// The default working set comes from the filesystem, not the log.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn default_validate_checks_files_logged_under_declared_kinds_or_not_at_all() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/good.md",
        &artifact("Good", "source_kind: pdf\n"),
    );
    // The first default run records every clean file.
    let (code, issues) = validate(&store, false);
    assert_eq!(code, Some(0), "{issues:#?}");

    // The September sequence: broken records written outside `dbmd write`,
    // logged under a store-declared kind or not at all, plus an ordinary
    // `update` entry for some other file.
    write_file(
        &store,
        "records/artifacts/declared.md",
        &artifact("Declared", "source-kind: pdf\n"),
    );
    write_file(
        &store,
        "records/artifacts/unlogged.md",
        &artifact("Unlogged", "source-kind: json\n"),
    );
    let log = std::fs::read_to_string(store.join("log.md"))
        .unwrap_or_else(|_| "---\ntype: log\n---\n".into());
    std::fs::write(
        store.join("log.md"),
        format!(
            "{log}\n## [2026-09-22 19:12] audit | records/artifacts/declared\nwrote\n\n\
             ## [2026-09-22 19:13] update | records/artifacts/good\nedited\n"
        ),
    )
    .unwrap();

    for run in 0..2 {
        let (code, issues) = validate(&store, false);
        assert_eq!(code, Some(6), "run {run}: {issues:#?}");
        for file in [
            "records/artifacts/declared.md",
            "records/artifacts/unlogged.md",
        ] {
            assert!(
                has(&issues, "SCHEMA_MISSING_REQUIRED", file),
                "run {run}: {file}: {issues:#?}"
            );
            assert!(
                has(&issues, "FM_KEY_NEAR_MISS", file),
                "run {run}: {file}: {issues:#?}"
            );
        }
    }

    // Fixed on disk: the next default run is clean again.
    write_file(
        &store,
        "records/artifacts/declared.md",
        &artifact("Declared", "source_kind: pdf\n"),
    );
    write_file(
        &store,
        "records/artifacts/unlogged.md",
        &artifact("Unlogged", "source_kind: json\n"),
    );
    let (code, issues) = validate(&store, false);
    assert_eq!(code, Some(0), "{issues:#?}");
}

#[test]
fn a_schema_change_rechecks_every_unchanged_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/a.md",
        &artifact("A", "source_kind: pdf\n"),
    );
    assert_eq!(validate(&store, false).0, Some(0));

    // The record is bound to DB.md's exact bytes: a new required field makes
    // the unchanged file invalid, and the default run must see it.
    let db = std::fs::read_to_string(store.join("DB.md")).unwrap();
    std::fs::write(
        store.join("DB.md"),
        db.replace(
            "- captured_at (date)\n",
            "- captured_at (date)\n- origin (required, string)\n",
        ),
    )
    .unwrap();
    let (code, issues) = validate(&store, false);
    assert_eq!(code, Some(6), "{issues:#?}");
    assert!(
        has(&issues, "SCHEMA_MISSING_REQUIRED", "records/artifacts/a.md"),
        "{issues:#?}"
    );
}

#[test]
fn an_unlogged_deletion_rechecks_the_files_that_link_to_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/target.md",
        &artifact("Target", "source_kind: pdf\n"),
    );
    write_file(
        &store,
        "records/artifacts/linker.md",
        &artifact("Linker", "source_kind: pdf\n")
            .replace("# T\n", "# T\n\nSee [[records/artifacts/target]].\n"),
    );
    assert_eq!(validate(&store, false).0, Some(0));

    std::fs::remove_file(store.join("records/artifacts/target.md")).unwrap();
    let (code, issues) = validate(&store, false);
    assert_eq!(code, Some(6), "{issues:#?}");
    assert!(
        has(&issues, "WIKI_LINK_BROKEN", "records/artifacts/linker.md"),
        "{issues:#?}"
    );
}

#[test]
fn a_typo_in_an_optional_key_is_reported() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/a.md",
        &artifact("A", "source_kind: pdf\ncaptured-at: 2026-09-22\n"),
    );
    for all in [false, true] {
        let (_, issues) = validate(&store, all);
        assert!(
            has(&issues, "FM_KEY_NEAR_MISS", "records/artifacts/a.md"),
            "all={all}: {issues:#?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Assets are cataloged with the record, or the write is refused.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn write_catalogs_a_declared_asset_and_refuses_a_missing_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    let body = body_file(tmp.path());
    let write = |name: &str| {
        dbmd()
            .current_dir(&store)
            .args([
                "--json",
                "write",
                &format!("records/artifacts/{name}.md"),
                "--type",
                "artifact",
                "--summary",
                "Deck",
                "--fm",
                "title=Deck",
                "--fm",
                "source_kind=pdf",
                "--fm",
                "asset=records/files/deck.pdf",
                "--body-file",
                &body,
            ])
            .output()
            .unwrap()
    };

    let missing = write("before-copy");
    assert_eq!(missing.status.code(), Some(6));
    let error: serde_json::Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(error["error"]["code"], "ASSET_NOT_FOUND");
    assert!(!store.join("records/artifacts/before-copy.md").exists());

    write_file(&store, "records/files/deck.pdf", "%PDF-1.7 test\n");
    let written = write("deck");
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let manifest = std::fs::read_to_string(store.join("assets.jsonl")).unwrap();
    assert!(
        manifest.contains("\"path\":\"records/files/deck.pdf\""),
        "{manifest}"
    );
    assert_eq!(validate(&store, true).0, Some(0));
}

#[test]
fn default_validate_reports_an_uncataloged_asset_written_outside_dbmd() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(&store, "records/files/deck.pdf", "%PDF-1.7 test\n");
    write_file(
        &store,
        "records/artifacts/deck.md",
        &artifact("Deck", "source_kind: pdf\nasset: records/files/deck.pdf\n"),
    );
    let (code, issues) = validate(&store, false);
    assert_eq!(code, Some(6), "{issues:#?}");
    assert!(
        has(&issues, "ASSET_UNDECLARED", "records/artifacts/deck.md"),
        "{issues:#?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Writes refuse what validation would reject.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn write_refuses_a_near_miss_key_and_names_the_fix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    let body = body_file(tmp.path());
    let output = dbmd()
        .current_dir(&store)
        .args([
            "--json",
            "write",
            "records/artifacts/deck.md",
            "--type",
            "artifact",
            "--summary",
            "Deck",
            "--fm",
            "title=Deck",
            "--fm",
            "source-kind=pdf",
            "--body-file",
            &body,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "WRITE_INVALID");
    let issues = error["error"]["details"]["issues"].as_array().unwrap();
    assert!(issues
        .iter()
        .any(|issue| issue["code"] == "FM_KEY_NEAR_MISS"
            && issue["suggestion"] == "rename `source-kind` to `source_kind`"));
    assert!(issues
        .iter()
        .any(|issue| issue["code"] == "SCHEMA_MISSING_REQUIRED"
            && issue["suggestion"] == "rename `source-kind` to `source_kind`"));
    assert!(
        !store.join("records/artifacts/deck.md").exists(),
        "nothing is written"
    );
}

#[test]
fn write_refuses_an_overlong_source_summary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    let body = body_file(tmp.path());
    let summary = format!("Realm capacity note {}", "x".repeat(210));
    let output = dbmd()
        .current_dir(&store)
        .args([
            "write",
            "sources/notes/2026/09/2026-09-11-realm.md",
            "--type",
            "note",
            "--summary",
            &summary,
            "--body-file",
            &body,
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("SUMMARY_TOO_LONG"));
    assert!(!store
        .join("sources/notes/2026/09/2026-09-11-realm.md")
        .exists());
}

#[test]
fn fm_set_refuses_a_new_finding_but_allows_a_repair() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/good.md",
        &artifact("Good", "source_kind: pdf\n"),
    );
    write_file(
        &store,
        "records/artifacts/broken.md",
        &artifact("Broken", ""),
    );

    let introduced = dbmd()
        .current_dir(&store)
        .args(["fm", "set", "records/artifacts/good.md", "Source_Kind=pdf"])
        .output()
        .unwrap();
    assert_eq!(introduced.status.code(), Some(6));
    assert!(
        !std::fs::read_to_string(store.join("records/artifacts/good.md"))
            .unwrap()
            .contains("Source_Kind")
    );

    // `broken.md` already lacks its required field; setting it is a repair.
    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "set",
            "records/artifacts/broken.md",
            "source_kind=json",
        ])
        .assert()
        .success();
    rebuild_indexes(&store);
    let (code, issues) = validate(&store, true);
    assert_eq!(code, Some(0), "{issues:#?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// One rule for choosing the store.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn every_store_command_accepts_dir_and_file_commands_find_their_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/a.md",
        &artifact("A", "source_kind: pdf\n"),
    );
    let outside = tmp.path();
    let dir = store.to_string_lossy().into_owned();

    // `dbmd log <kind> <object> --dir` (the September script's crash).
    dbmd()
        .current_dir(outside)
        .args([
            "log",
            "audit",
            "records/artifacts/a",
            "--dir",
            &dir,
            "-m",
            "note",
        ])
        .assert()
        .success();
    // `dbmd validate --dir` (the second crash), and the positional form.
    dbmd()
        .current_dir(outside)
        .args(["validate", "--dir", &dir])
        .assert()
        .success();
    dbmd()
        .current_dir(outside)
        .args(["validate", &dir])
        .assert()
        .success();
    // Appending from a subdirectory finds the store above it.
    let sub = store.join("records/artifacts");
    dbmd()
        .current_dir(&sub)
        .args([
            "log",
            "note",
            "records/artifacts/a",
            "-m",
            "from a subdirectory",
        ])
        .assert()
        .success();
    // `dbmd fm set` on an absolute path from outside every store (the third).
    let file = store.join("records/artifacts/a.md");
    dbmd()
        .current_dir(outside)
        .args([
            "fm",
            "set",
            &file.to_string_lossy(),
            "summary=A, edited from outside",
        ])
        .assert()
        .success();
    let log = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert!(log.contains("audit | records/artifacts/a") && log.contains("from a subdirectory"));
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("A, edited from outside"));
}

// ─────────────────────────────────────────────────────────────────────────────
// A store can make every warning blocking.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn blocking_warnings_policy_fails_validation_on_any_warning() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/a.md",
        &artifact("A", "source_kind: pdf\ncaptured-at: 2026-09-22\n"),
    );
    rebuild_indexes(&store);
    let (code, issues) = validate(&store, true);
    assert_eq!(
        code,
        Some(0),
        "a warning alone does not fail by default: {issues:#?}"
    );

    let db = std::fs::read_to_string(store.join("DB.md")).unwrap();
    std::fs::write(
        store.join("DB.md"),
        db.replace(
            "### Validation log kinds",
            "### Blocking warnings\n- all\n\n### Validation log kinds",
        ),
    )
    .unwrap();
    for all in [false, true] {
        let (code, issues) = validate(&store, all);
        assert_eq!(code, Some(6), "all={all}: {issues:#?}");
        assert!(issues
            .iter()
            .any(|issue| issue["code"] == "FM_KEY_NEAR_MISS" && issue["severity"] == "error"));
    }

    std::fs::write(
        store.join("DB.md"),
        db.replace(
            "### Validation log kinds",
            "### Blocking warnings\n- some\n\n### Validation log kinds",
        ),
    )
    .unwrap();
    let (code, issues) = validate(&store, true);
    assert_eq!(code, Some(6));
    assert!(issues
        .iter()
        .any(|issue| issue["code"] == "VALIDATION_POLICY_INVALID"));
}

#[test]
fn the_validation_record_stays_local_and_out_of_version_control() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join("store");
    store_with_schema(&store);
    write_file(
        &store,
        "records/artifacts/a.md",
        &artifact("A", "source_kind: pdf\n"),
    );
    assert_eq!(validate(&store, false).0, Some(0));
    assert!(store.join(".dbmd/validate-state.json").is_file());
    let ignore = std::fs::read_to_string(store.join(".dbmd/.gitignore")).unwrap();
    assert!(ignore.contains("validate-state.json") && ignore.contains(".gitignore"));
    // The record never becomes store content.
    let (_, issues) = validate(&store, true);
    assert!(issues
        .iter()
        .all(|issue| !issue["file"].as_str().unwrap_or("").contains(".dbmd")));
}
