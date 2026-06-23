//! Integration tests for `dbmd sections <file>`.
//!
//! `sections` lists the `##`+ headings of one file. Byte-exact behavior is
//! pinned with synthetic temp fixtures (full control over the body); the real
//! corpus-a happy-path file is used to confirm the extractor sees the right
//! headings in a realistic document.

mod common;

use common::{corpus_a, dbmd, write_file};

/// A file with two `##` sections and one nested `###` — the canonical shape.
const TWO_SECTIONS: &str = "\
---
type: wiki-page
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

#[test]
fn text_lists_headings_indented_by_depth() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = write_file(tmp.path(), "page.md", TWO_SECTIONS);

    let out = dbmd().arg("sections").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    // `##` flush-left, `###` indented two spaces; H1 is never listed. Line
    // numbers are source-relative: the 4-line frontmatter (`---`, two YAML
    // lines, `---`) precedes the body, so `## Timeline` is source line 10.
    let expected = "Timeline  (L10)\n  Sub-detail  (L14)\nCommercials  (L18)\n";
    assert_eq!(stdout, expected);
}

#[test]
fn json_emits_heading_level_line_array() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = write_file(tmp.path(), "page.md", TWO_SECTIONS);

    let out = dbmd()
        .arg("--json")
        .arg("sections")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    // Parse-and-compare (rather than byte-snapshot) so the assertion is robust
    // to pretty-printer whitespace while still pinning every value.
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let expected = serde_json::json!([
        { "heading": "Timeline", "level": 2, "line": 10 },
        { "heading": "Sub-detail", "level": 3, "line": 14 },
        { "heading": "Commercials", "level": 2, "line": 18 },
    ]);
    assert_eq!(parsed, expected);
}

#[test]
fn file_with_no_h2_sections_prints_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Only an H1 and prose — no `##`+ headings.
    let file = write_file(
        tmp.path(),
        "flat.md",
        "---\ntype: wiki-page\nsummary: s\n---\n\n# Only a title\n\nJust prose.\n",
    );

    let out = dbmd().arg("sections").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "", "no sections → empty stdout (pipe-safe)");
}

#[test]
fn fenced_code_headings_are_not_sections() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = write_file(
        tmp.path(),
        "code.md",
        "---\ntype: wiki-page\nsummary: s\n---\n\n## Real\n\n```\n## not a heading\n```\n\n## Also real\n",
    );

    let out = dbmd().arg("sections").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    // The `## not a heading` inside the fence must not appear.
    assert!(stdout.contains("Real"));
    assert!(stdout.contains("Also real"));
    assert!(
        !stdout.contains("not a heading"),
        "a `##` inside a code fence is not a section: {stdout:?}"
    );
    assert_eq!(stdout.lines().count(), 2, "exactly the two real headings");
}

#[test]
fn corpus_a_conclusion_record_sections_are_seen() {
    // The committed happy-path conclusion record has `## Timeline` and `## Commercials`.
    let file = corpus_a().join("records/projects/northstar-renewal.md");

    let out = dbmd()
        .arg("--json")
        .arg("sections")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON array");
    let arr = parsed.as_array().expect("array");

    let headings: Vec<&str> = arr
        .iter()
        .filter_map(|s| s.get("heading").and_then(|h| h.as_str()))
        .collect();
    assert_eq!(
        headings,
        vec!["Timeline", "Commercials"],
        "the two H2 sections, in document order"
    );
    // Every entry is level 2 (the page nests no deeper H3s under these H2s).
    assert!(arr
        .iter()
        .all(|s| s.get("level").and_then(|l| l.as_u64()) == Some(2)));
}

#[test]
fn missing_file_is_runtime_error_nonzero_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.md");

    dbmd()
        .arg("sections")
        .arg(&missing)
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime
}
