//! Integration tests for `dbmd links <target>`.
//!
//! `links` lists every incoming wiki-link to a target file (its dependents),
//! via the embedded-ripgrep backlink scan in `dbmd-core`. Byte-exact behavior
//! is pinned with a synthetic temp store; corpus-a confirms realistic backlink
//! resolution (and that the `.jsonl` sidecars are never scanned — only `.md`).

mod common;

use common::{corpus_a, dbmd, write_db_md, write_file};

/// Build a tiny store with a known backlink shape into `target`.
fn synthetic_store() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    write_db_md(root);
    // The target.
    write_file(
        root,
        "records/companies/acme.md",
        "---\ntype: company\nsummary: Acme\n---\n\n# Acme\n",
    );
    // Three linkers, in different layers, each a different accepted spelling.
    write_file(
        root,
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: s\n---\n\nWorks at [[records/companies/acme]].\n",
    );
    write_file(
        root,
        "wiki/people/sarah.md",
        "---\ntype: wiki-page\nsummary: s\n---\n\nSee [[records/companies/acme|Acme Inc]].\n",
    );
    write_file(
        root,
        "sources/emails/2026/05/intro.md",
        "---\ntype: email\nsummary: s\n---\n\nRe [[records/companies/acme.md]].\n",
    );
    // A non-linker, and a longer path that must NOT match on a prefix.
    write_file(
        root,
        "wiki/people/bob.md",
        "---\ntype: wiki-page\nsummary: s\n---\n\nNo links here.\n",
    );
    write_file(
        root,
        "records/contacts/jr.md",
        "---\ntype: contact\nsummary: s\n---\n\n[[records/companies/acme-holdings]]\n",
    );
    tmp
}

#[test]
fn text_lists_every_accepted_spelling_sorted() {
    let tmp = synthetic_store();

    let out = dbmd()
        .arg("links")
        .arg("records/companies/acme")
        .arg("--dir")
        .arg(tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    // All three spellings ([[x]], [[x|d]], [[x.md]]) resolve; sorted by path;
    // the non-linker and the prefix-only longer path are excluded.
    let expected = "records/contacts/sarah.md\n\
                    sources/emails/2026/05/intro.md\n\
                    wiki/people/sarah.md\n";
    assert_eq!(stdout, expected);
}

#[test]
fn json_reports_target_count_and_links() {
    let tmp = synthetic_store();

    let out = dbmd()
        .arg("--json")
        .arg("links")
        .arg("records/companies/acme")
        .arg("--dir")
        .arg(tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let expected = serde_json::json!({
        "target": "records/companies/acme",
        "count": 3,
        "links": [
            "records/contacts/sarah.md",
            "sources/emails/2026/05/intro.md",
            "wiki/people/sarah.md",
        ],
    });
    assert_eq!(parsed, expected);
}

#[test]
fn target_with_no_backlinks_is_empty_success() {
    let tmp = synthetic_store();
    // A real file nobody links to.
    let out = dbmd()
        .arg("links")
        .arg("wiki/people/bob")
        .arg("--dir")
        .arg(tmp.path())
        .assert()
        .success();
    assert_eq!(
        String::from_utf8(out.get_output().stdout.clone()).unwrap(),
        "",
        "no backlinks → empty stdout, exit 0"
    );
}

#[test]
fn corpus_a_backlinks_resolve_md_only_never_jsonl() {
    // The committed conclusion record is linked from several files; the scan walks
    // every `.md` (incl. catalogs + log.md) but never the `.jsonl` sidecars.
    let out = dbmd()
        .arg("--json")
        .arg("links")
        .arg("records/projects/northstar-renewal")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let links: Vec<&str> = parsed["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    // Known linkers (content + a catalog + the log) MUST appear.
    for expected in [
        "records/companies/northstar.md",
        "records/contacts/sarah-chen.md",
        "records/synthesis/2026-renewal-plan.md",
        "records/projects/index.md",
        "log.md",
    ] {
        assert!(
            links.contains(&expected),
            "expected backlink `{expected}` in {links:?}"
        );
    }
    // No `.jsonl` sidecar is ever a backlink (the scan is `.md`-only), and the
    // log archive tree under `log/` is excluded.
    assert!(
        links.iter().all(|l| l.ends_with(".md")),
        "only .md files are scanned: {links:?}"
    );
    assert!(
        !links.iter().any(|l| l.starts_with("log/")),
        "the log/ archive dir is excluded: {links:?}"
    );
}

#[test]
fn not_a_store_is_exit_3() {
    let tmp = tempfile::TempDir::new().unwrap();
    // No DB.md → NOT_A_STORE.
    dbmd()
        .arg("links")
        .arg("records/x")
        .arg("--dir")
        .arg(tmp.path())
        .assert()
        .failure()
        .code(3); // ExitCode::NotAStore
}
