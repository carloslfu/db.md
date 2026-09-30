// SPDX-License-Identifier: Apache-2.0

//! Regression gates: explicit policy must not weaken integrity validation.
use std::fs;

use dbmd_core::validate::{codes, validate_all, validate_working_set};
use dbmd_core::{Index, Issue, Severity, Store};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const SCHEMA: &str =
    "\n## Schemas\n### evidence\n- original_path (string)\n- unique: original_path\n";

fn file(summary: &str, extra: &str, body: &str) -> String {
    format!("---\ntype: evidence\ncreated: 2026-09-04T00:00:00Z\nupdated: 2026-09-04T00:00:00Z\nsummary: {summary}\n{extra}---\n{body}\n")
}

fn fixture(policy: &str, schema: &str, files: &[(&str, String)]) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("DB.md"),
        format!(
            "---\ntype: db-md\nscope: personal\nowner: Test\n---\n## Policies\n{policy}{schema}"
        ),
    )
    .unwrap();
    for (path, text) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    Index::rebuild_all(&store).unwrap();
    (dir, store)
}

fn count(issues: &[Issue], code: &str, severity: Severity) -> usize {
    issues
        .iter()
        .filter(|i| i.code == code && i.severity == severity)
        .count()
}

fn preserved(path: &str, text: &str) -> String {
    format!(
        "### Preserved source summaries\n- {}\n",
        serde_json::json!({
            "path": path, "sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
            "reason": "Preserve captured evidence bytes"
        })
    )
}

#[test]
fn defaults_still_report_all_three_warning_classes() {
    let (dir, store) = fixture(
        "",
        SCHEMA,
        &[("sources/raw/a.md", file(&"x".repeat(201), "", ""))],
    );
    fs::write(
        dir.path().join("log.md"),
        "---\ntype: log\n---\n## [2026-09-04 00:00] synthesis | sources/raw/a.md\n",
    )
    .unwrap();
    let issues = validate_all(&store).unwrap();
    for code in [
        codes::DB_MD_SCHEMA_FIELD,
        codes::LOG_UNKNOWN_KIND,
        codes::SUMMARY_TOO_LONG,
    ] {
        assert_eq!(count(&issues, code, Severity::Warning), 1, "{issues:?}");
    }
}

#[test]
fn declared_log_vocabulary_leaves_unknown_labels_and_bad_timestamps_visible() {
    let (dir, store) = fixture("### Validation log kinds\n- synthesis\n", "", &[]);
    fs::write(dir.path().join("log.md"), "---\ntype: log\n---\n## [2026-09-04 00:00] synthesis | x\n## [2026-09-04 00:01] typo | x\n## [wrong] synthesis | x\n").unwrap();
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::LOG_UNKNOWN_KIND, Severity::Warning),
        1
    );
    assert_eq!(count(&issues, codes::LOG_BAD_TIMESTAMP, Severity::Error), 1);
}

#[test]
fn fenced_examples_and_nested_prose_do_not_declare_log_kinds() {
    let (_dir, store) = fixture(
        "### Validation log kinds\n- synthesis\n```text\n- example\n```\n#### Examples\n- nested\n",
        "",
        &[],
    );
    assert!(store
        .config
        .validation_policy
        .recognizes_log_kind("synthesis"));
    assert!(!store
        .config
        .validation_policy
        .recognizes_log_kind("example"));
    assert!(!store.config.validation_policy.recognizes_log_kind("nested"));
}

#[test]
fn policy_entry_limit_is_enforced_without_wildcard_acceptance() {
    let mut policy = String::from("### Validation log kinds\n");
    for n in 0..257 {
        policy.push_str(&format!("- kind{n}\n"));
    }
    let (_dir, store) = fixture(&policy, "", &[]);
    assert_eq!(
        count(
            &validate_all(&store).unwrap(),
            codes::VALIDATION_POLICY_INVALID,
            Severity::Error
        ),
        1
    );
    assert!(!store
        .config
        .validation_policy
        .recognizes_log_kind("kind256"));
}

#[test]
fn optional_uniqueness_keeps_duplicate_detection_and_missing_values_valid() {
    let policy = "### Optional unique keys\n- {\"type\":\"evidence\",\"fields\":[\"original_path\"],\"reason\":\"Some evidence has no original path\"}\n";
    let (_dir, store) = fixture(
        policy,
        SCHEMA,
        &[
            (
                "records/evidence/a.md",
                file("a", "original_path: same\n", ""),
            ),
            (
                "records/evidence/b.md",
                file("b", "original_path: same\n", ""),
            ),
            ("records/evidence/c.md", file("c", "", "")),
        ],
    );
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::DB_MD_SCHEMA_FIELD, Severity::Warning),
        0
    );
    assert_eq!(
        count(&issues, codes::DUP_UNIQUE_KEY, Severity::Warning),
        1,
        "{issues:?}"
    );
    assert!(!issues.iter().any(Issue::is_error), "{issues:?}");
}

#[test]
fn optional_policy_cannot_name_an_absent_constraint_or_undeclared_field() {
    for schema in ["", "\n## Schemas\n### evidence\n- unique: original_path\n"] {
        let (_dir, store) = fixture("### Optional unique keys\n- {\"type\":\"evidence\",\"fields\":[\"original_path\"],\"reason\":\"intentional\"}\n", schema, &[]);
        assert_eq!(
            count(
                &validate_all(&store).unwrap(),
                codes::VALIDATION_POLICY_INVALID,
                Severity::Error
            ),
            1
        );
    }
}

#[test]
fn preserved_summary_is_informational_but_broken_links_still_fail() {
    let text = file(&"x".repeat(201), "", "[[records/missing]]");
    let (_dir, store) = fixture(
        &preserved("sources/raw/a.md", &text),
        "",
        &[("sources/raw/a.md", text)],
    );
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::SUMMARY_TOO_LONG, Severity::Warning),
        0
    );
    assert_eq!(count(&issues, codes::SUMMARY_TOO_LONG, Severity::Info), 1);
    assert_eq!(count(&issues, codes::WIKI_LINK_BROKEN, Severity::Error), 1);
}

#[test]
fn source_exceptions_are_path_and_hash_bound_not_blanket_suppressions() {
    let text = file(&"x".repeat(201), "", "");
    let (dir, store) = fixture(
        &preserved("sources/raw/a.md", &text),
        "",
        &[
            ("sources/raw/a.md", text.clone()),
            ("sources/raw/b.md", text.clone()),
        ],
    );
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::SUMMARY_TOO_LONG, Severity::Warning),
        1
    );
    assert_eq!(count(&issues, codes::SUMMARY_TOO_LONG, Severity::Info), 1);
    fs::write(dir.path().join("sources/raw/a.md"), text.replace('x', "y")).unwrap();
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::VALIDATION_POLICY_INVALID, Severity::Error),
        1
    );
    assert_eq!(count(&issues, codes::SUMMARY_TOO_LONG, Severity::Info), 0);
    assert_eq!(
        count(&issues, codes::SUMMARY_TOO_LONG, Severity::Warning),
        2
    );
}

#[test]
fn working_set_applies_preserved_summary_policy() {
    let text = file(&"x".repeat(201), "", "");
    let (dir, store) = fixture(
        &preserved("sources/raw/a.md", &text),
        "",
        &[("sources/raw/a.md", text)],
    );
    fs::write(
        dir.path().join("log.md"),
        "---\ntype: log\n---\n## [2026-09-04 01:00] update | sources/raw/a.md\n",
    )
    .unwrap();
    let since = chrono::DateTime::parse_from_rfc3339("2026-09-04T00:00:00Z").unwrap();
    let issues = validate_working_set(&store, Some(since)).unwrap();
    assert_eq!(count(&issues, codes::SUMMARY_TOO_LONG, Severity::Info), 1);
    assert_eq!(
        count(&issues, codes::SUMMARY_TOO_LONG, Severity::Warning),
        0
    );
}

#[test]
fn missing_source_is_a_policy_error_even_in_an_empty_explicit_working_set() {
    let policy = preserved("sources/raw/missing.md", &file(&"x".repeat(201), "", ""));
    let (_dir, store) = fixture(&policy, "", &[]);
    let since = chrono::DateTime::parse_from_rfc3339("2026-09-04T00:00:00Z").unwrap();
    assert_eq!(
        count(
            &validate_working_set(&store, Some(since)).unwrap(),
            codes::VALIDATION_POLICY_INVALID,
            Severity::Error
        ),
        1
    );
}

#[test]
fn stale_short_summary_exception_fails_even_with_matching_hash() {
    let text = file("short", "", "");
    let (_dir, store) = fixture(
        &preserved("sources/raw/a.md", &text),
        "",
        &[("sources/raw/a.md", text)],
    );
    assert_eq!(
        count(
            &validate_all(&store).unwrap(),
            codes::VALIDATION_POLICY_INVALID,
            Severity::Error
        ),
        1
    );
}

#[test]
fn malformed_wildcard_and_duplicate_policies_fail_closed() {
    for policy in [
        "### Validation log kinds\n- *\n",
        "### Validation log kinds\n- synthesis\n- synthesis\n",
        "### Optional unique keys\n- {}\n",
        "### Preserved source summaries\n- {}\n",
        "### Preserved source summaries\n- {\"code\":\"WIKI_LINK_BROKEN\"}\n",
    ] {
        let (_dir, store) = fixture(policy, "", &[]);
        assert_eq!(
            count(
                &validate_all(&store).unwrap(),
                codes::VALIDATION_POLICY_INVALID,
                Severity::Error
            ),
            1,
            "{policy}"
        );
    }
    for path in [
        "records/a.md",
        "sources/../a.md",
        "sources/*.md",
        "sources/x//a.md",
        "sources/index.md",
        "sources\\a.md",
    ] {
        let (_dir, store) = fixture(&preserved(path, "x"), "", &[]);
        assert_eq!(
            count(
                &validate_all(&store).unwrap(),
                codes::VALIDATION_POLICY_INVALID,
                Severity::Error
            ),
            1,
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn source_acknowledgement_never_follows_a_symlink() {
    let text = file(&"x".repeat(201), "", "");
    let (dir, store) = fixture(
        &preserved("sources/raw/link.md", &text),
        "",
        &[("records/owned/a.md", text)],
    );
    fs::create_dir_all(dir.path().join("sources/raw")).unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("records/owned/a.md"),
        dir.path().join("sources/raw/link.md"),
    )
    .unwrap();
    let issues = validate_all(&store).unwrap();
    assert_eq!(
        count(&issues, codes::VALIDATION_POLICY_INVALID, Severity::Error),
        1
    );
    assert_eq!(count(&issues, codes::SUMMARY_TOO_LONG, Severity::Info), 0);
}
