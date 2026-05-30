//! Integration tests for `dbmd query`.
//!
//! `query` is a frontmatter filter resolved against the `index.jsonl` sidecar
//! (never a whole-store parse). The committed corpus-a sidecars are the
//! fixtures: `records/contacts/index.jsonl` holds four contacts, etc. Tests
//! assert the matched path set and the returned record fields, plus the flag
//! semantics (`--type`, `--where`, `--in`, `--limit`).

mod common;

use std::collections::BTreeSet;

use common::{corpus_a, dbmd};

/// Run `dbmd query <args> --dir corpus-a` and return stdout lines as a set.
fn query_paths(args: &[&str]) -> BTreeSet<String> {
    let out = dbmd()
        .arg("query")
        .args(args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    stdout.lines().map(|l| l.to_string()).collect()
}

#[test]
fn type_filter_returns_that_types_records() {
    let got = query_paths(&["--type", "contact"]);
    let expected: BTreeSet<String> = [
        "records/contacts/david-kim.md",
        "records/contacts/elena-rodriguez.md",
        "records/contacts/marcus-okafor.md",
        "records/contacts/sarah-chen.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        got, expected,
        "all four contacts, from the contacts sidecar"
    );
}

#[test]
fn type_filter_excludes_other_types() {
    let companies = query_paths(&["--type", "company"]);
    // The companies sidecar; no contact leaks in.
    assert!(companies.contains("records/companies/northstar.md"));
    assert!(
        !companies.iter().any(|p| p.starts_with("records/contacts/")),
        "a company query must not return contacts: {companies:?}"
    );
}

#[test]
fn where_narrows_within_a_type() {
    // Only contacts whose `company` field points at Northstar.
    let northstar = query_paths(&[
        "--type",
        "contact",
        "--where",
        "company=[[records/companies/northstar]]",
    ]);
    assert!(northstar.contains("records/contacts/sarah-chen.md"));
    assert!(northstar.contains("records/contacts/elena-rodriguez.md"));
    assert!(northstar.contains("records/contacts/marcus-okafor.md"));
    // David Kim's company is Acme — excluded.
    assert!(
        !northstar.contains("records/contacts/david-kim.md"),
        "the where clause excludes the Acme contact: {northstar:?}"
    );
}

#[test]
fn where_on_universal_status_field() {
    // `status` is a universal-contract column on the record; all four contacts
    // are active in the corpus.
    let active = query_paths(&["--type", "contact", "--where", "status=active"]);
    assert_eq!(active.len(), 4, "every contact is active: {active:?}");
}

#[test]
fn limit_caps_the_result_count() {
    let two = query_paths(&["--type", "contact", "--limit", "2"]);
    assert_eq!(two.len(), 2, "limit caps the path-sorted result set");
    // The cap is applied after the path sort, so it is the two lexicographically
    // smallest paths — deterministic.
    let expected: BTreeSet<String> = [
        "records/contacts/david-kim.md",
        "records/contacts/elena-rodriguez.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(two, expected);
}

#[test]
fn in_layer_scopes_results() {
    // `--type wiki-page --in records` must be empty (wiki-pages live in wiki/).
    let in_records = query_paths(&["--type", "wiki-page", "--in", "records"]);
    assert!(
        in_records.is_empty(),
        "wiki-page records are not under records/: {in_records:?}"
    );
    // The same type scoped to its real layer is non-empty.
    let in_wiki = query_paths(&["--type", "wiki-page", "--in", "wiki"]);
    assert!(!in_wiki.is_empty(), "wiki-pages live under wiki/");
}

#[test]
fn json_returns_full_records_with_fields() {
    let out = dbmd()
        .arg("--json")
        .arg("query")
        .arg("--type")
        .arg("contact")
        .arg("--where")
        .arg("email=david.kim@acme.com")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 1, "one contact matches the email");

    let rec = &arr[0];
    // The full sidecar record comes back: path + type + summary + type-specific
    // fields (e.g. `company`, `name`, `role`) verbatim.
    assert_eq!(rec["path"], "records/contacts/david-kim.md");
    assert_eq!(rec["type"], "contact");
    assert_eq!(rec["name"], "David Kim");
    assert_eq!(rec["company"], "[[records/companies/acme]]");
    assert!(
        rec["summary"]
            .as_str()
            .unwrap()
            .contains("Account Executive"),
        "summary is carried verbatim from the sidecar"
    );
}

#[test]
fn no_match_is_empty_success() {
    let got = query_paths(&["--type", "contact", "--where", "status=nonexistent"]);
    assert!(got.is_empty());
}

#[test]
fn bad_where_clause_is_runtime_error() {
    // A `--where` with no `=` is a deterministic usage-class runtime failure.
    dbmd()
        .arg("query")
        .arg("--type")
        .arg("contact")
        .arg("--where")
        .arg("not-a-pair")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .failure()
        .code(1);
}

#[test]
fn bad_layer_is_runtime_error() {
    dbmd()
        .arg("query")
        .arg("--type")
        .arg("contact")
        .arg("--in")
        .arg("nonsense")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .failure()
        .code(1);
}

#[test]
fn not_a_store_is_exit_3() {
    let tmp = tempfile::TempDir::new().unwrap();
    dbmd()
        .arg("query")
        .arg("--type")
        .arg("contact")
        .arg("--dir")
        .arg(tmp.path())
        .assert()
        .failure()
        .code(3);
}
