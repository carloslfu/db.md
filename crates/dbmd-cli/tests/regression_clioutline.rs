//! Regression tests for `dbmd outline <file>` — confirmed launch-readiness
//! finding #27.
//!
//! Bug: `outline` opened the store with `Store::open_strict`, so running it
//! outside a db.md store failed `NOT_A_STORE` (exit 3) even though listing one
//! file's headings reads no `DB.md`. Its twin `dbmd sections <file>` reads any
//! file directly and succeeds (exit 0). The two single-file views disagreed.
//!
//! Fix: `outline` now reads the single file directly, no store required, so
//! both commands behave identically. These tests reconstruct the exact trigger
//! and would FAIL against the pre-fix code (which exited 3 with `NOT_A_STORE`).

mod common;

use common::{dbmd, write_file};

/// The canonical two-section shape: two `##` and one nested `###`.
const TWO_SECTIONS: &str = "\
---
type: note
summary: a page
---

# Title

Intro paragraph.

## Timeline

- a point

### Sub-detail

more.

## Commercials

closing.
";

/// THE BUG: from a directory with NO `DB.md`, `dbmd outline ./file.md` must
/// succeed and print the outline — exactly like `dbmd sections`. Pre-fix this
/// exited 3 (`NOT_A_STORE`) because `outline` called `Store::open_strict`.
#[test]
fn regression_outline_reads_single_file_without_a_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Deliberately NO DB.md in this directory — the offending precondition.
    assert!(
        !tmp.path().join("DB.md").exists(),
        "fixture must have no DB.md to reproduce the bug"
    );
    let file = write_file(tmp.path(), "notes.md", TWO_SECTIONS);

    let out = dbmd().arg("outline").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    // Nested text outline: `##` flush-left, `###` indented two spaces; H1 is
    // never listed. Pre-fix this body never ran (exit 3 before any output).
    assert_eq!(stdout, "Timeline\n  Sub-detail\nCommercials\n");
}

/// The `--json` view must also work outside a store and carry the structured
/// `{file, sections:[{heading, level, line}]}` shape with source-relative lines.
#[test]
fn regression_outline_json_works_without_a_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(!tmp.path().join("DB.md").exists());
    let file = write_file(tmp.path(), "notes.md", TWO_SECTIONS);

    let out = dbmd()
        .arg("--json")
        .arg("outline")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON object");
    let sections = parsed.get("sections").and_then(|s| s.as_array()).unwrap();
    // Source-relative 1-based lines: the 4-line frontmatter precedes the body,
    // so `## Timeline` is source line 10 — matching `dbmd sections`.
    assert_eq!(
        sections,
        &serde_json::json!([
            { "heading": "Timeline", "level": 2, "line": 10 },
            { "heading": "Sub-detail", "level": 3, "line": 14 },
            { "heading": "Commercials", "level": 2, "line": 18 },
        ])
        .as_array()
        .unwrap()
        .clone()
    );
}

/// `outline` and `sections` must now agree on the same headings for the same
/// store-less file — the consistency the finding is about. Both succeed; the
/// `(heading, level, line)` triples match exactly.
#[test]
fn regression_outline_and_sections_agree_without_a_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(!tmp.path().join("DB.md").exists());
    let file = write_file(tmp.path(), "notes.md", TWO_SECTIONS);

    let outline_out = dbmd()
        .arg("--json")
        .arg("outline")
        .arg(&file)
        .assert()
        .success();
    let outline_json: serde_json::Value =
        serde_json::from_slice(&outline_out.get_output().stdout).unwrap();
    let outline_sections = outline_json.get("sections").unwrap().as_array().unwrap();

    let sections_out = dbmd()
        .arg("--json")
        .arg("sections")
        .arg(&file)
        .assert()
        .success();
    let sections_json: serde_json::Value =
        serde_json::from_slice(&sections_out.get_output().stdout).unwrap();
    let sections_arr = sections_json.as_array().unwrap();

    assert_eq!(
        outline_sections, sections_arr,
        "outline and sections must report identical (heading, level, line) triples"
    );
}

/// A file with no frontmatter must still outline cleanly (exit 0) outside a
/// store — outline is lenient by design and never fails on missing frontmatter.
/// Lines are counted from the first line when there is no frontmatter to strip.
#[test]
fn regression_outline_is_lenient_about_missing_frontmatter() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(!tmp.path().join("DB.md").exists());
    // No `---` fence at all — pure body.
    let file = write_file(tmp.path(), "plain.md", "# Title\n\n## One\n\ntext\n");

    let out = dbmd().arg("outline").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "One\n");
}

/// A missing file is still a clean runtime error (exit 1), matching the twin
/// `dbmd sections` — not `NOT_A_STORE` (3) and not a panic.
#[test]
fn regression_outline_missing_file_is_runtime_error_exit_1() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.md");

    dbmd()
        .arg("outline")
        .arg(&missing)
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime, NOT NotAStore (3)
}
