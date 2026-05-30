//! Integration tests for `dbmd validate`.
//!
//! Intent-derived per `tests/corpora/*/EXPECTED/`: corpus-a (canonical) MUST
//! validate clean (exit 0); corpus-b (designed-to-fail) MUST report errors and
//! exit non-zero (6); a no-`DB.md` directory MUST surface a single
//! `NOT_A_STORE` issue and exit non-zero. The exact issue *count/wording* is the
//! engine's (`dbmd-core`); these tests assert the contract that holds regardless
//! of wording — exit codes, the JSON envelope shape, and which issue *codes*
//! fire on a fixture — plus byte-exact behavior on synthetic single-issue
//! stores. They never copy the tool's own emitted prose.

mod common;

use std::collections::BTreeSet;

use common::{corpus_a, corpus_b, dbmd, write_db_md, write_file};

/// Parse a `--json` validate run's stdout into its envelope.
fn run_validate_json(dir: &std::path::Path, all: bool) -> (i32, serde_json::Value) {
    let mut cmd = dbmd();
    cmd.arg("--json").arg("validate");
    if all {
        cmd.arg("--all");
    }
    cmd.arg(dir);
    let output = cmd.output().expect("run dbmd validate");
    let code = output.status.code().expect("process exited normally");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("valid JSON: {e}\n{stdout}"));
    (code, json)
}

/// The set of issue `code`s in a validate JSON envelope.
fn codes(json: &serde_json::Value) -> BTreeSet<String> {
    json["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .filter_map(|i| i.get("code").and_then(|c| c.as_str()).map(String::from))
        .collect()
}

// ── corpus-a: the canonical store validates clean ───────────────────────────

#[test]
fn corpus_a_all_is_clean_exit_zero() {
    let (code, json) = run_validate_json(&corpus_a(), true);
    assert_eq!(code, 0, "the canonical store has no errors");
    assert_eq!(json["summary"]["errors"], 0);
    assert_eq!(
        json["issues"].as_array().unwrap().len(),
        0,
        "corpus-a --all emits zero issues: {}",
        json["issues"]
    );
    assert_eq!(json["scope"], "all");
}

#[test]
fn corpus_a_working_set_default_is_clean() {
    // The default (no --all) is the working-set scope; on the clean store it is
    // a subset of the clean full sweep, so it must also be error-free.
    let (code, json) = run_validate_json(&corpus_a(), false);
    assert_eq!(code, 0);
    assert_eq!(json["scope"], "working-set");
    assert_eq!(json["summary"]["errors"], 0);
}

// ── corpus-b: the designed-to-fail store reports errors ─────────────────────

#[test]
fn corpus_b_all_reports_errors_exit_six() {
    let (code, json) = run_validate_json(&corpus_b(), true);
    assert_eq!(code, 6, "errors present → ExitCode::ValidationFailed");
    let errors = json["summary"]["errors"].as_u64().unwrap();
    assert!(
        errors > 0,
        "corpus-b is seeded with many errors, got {errors}"
    );
    // The summary tallies must add up.
    let total = json["summary"]["total"].as_u64().unwrap();
    let warnings = json["summary"]["warnings"].as_u64().unwrap();
    let info = json["summary"]["info"].as_u64().unwrap();
    assert_eq!(
        total,
        errors + warnings + info,
        "summary totals are consistent"
    );
}

#[test]
fn corpus_b_fires_the_seeded_codes() {
    let (_code, json) = run_validate_json(&corpus_b(), true);
    let fired = codes(&json);
    // A representative slice of the codes the corpus-b EXPECTED contract seeds
    // and the engine emits — each owned by a distinct fixture file. (We assert a
    // robust subset, not the full count, since wording/line detail is the
    // engine's and may evolve; the contract here is "these classes are caught".)
    for code in [
        "FM_MISSING_TYPE",
        "FM_MALFORMED_YAML",
        "FM_BAD_TIMESTAMP",
        "SUMMARY_MISSING",
        "SUMMARY_EMPTY",
        "SUMMARY_MULTILINE",
        "SUMMARY_TOO_LONG",
        "WIKI_LINK_SHORT_FORM",
        "WIKI_LINK_BROKEN",
        "DUP_ID",
        "SCHEMA_MISSING_REQUIRED",
        "SCHEMA_ENUM_VIOLATION",
        "SCHEMA_LINK_PREFIX_MISMATCH",
        "LOG_BAD_TIMESTAMP",
        "INDEX_MISSING",
    ] {
        assert!(
            fired.contains(code),
            "expected `{code}` in corpus-b --all; got {fired:?}"
        );
    }
}

#[test]
fn corpus_b_every_issue_has_the_contract_shape() {
    let (_code, json) = run_validate_json(&corpus_b(), true);
    for issue in json["issues"].as_array().unwrap() {
        // The issue object shape from EXPECTED/README.md.
        assert!(issue.get("severity").and_then(|v| v.as_str()).is_some());
        assert!(issue.get("code").and_then(|v| v.as_str()).is_some());
        assert!(issue.get("file").and_then(|v| v.as_str()).is_some());
        // `line` and `key` are present as keys (null is allowed).
        assert!(
            issue.get("line").is_some(),
            "line key present (may be null)"
        );
        assert!(issue.get("key").is_some(), "key key present (may be null)");
        assert!(issue.get("message").and_then(|v| v.as_str()).is_some());
        assert!(issue.get("related").and_then(|v| v.as_array()).is_some());
        let severity = issue["severity"].as_str().unwrap();
        assert!(
            matches!(severity, "error" | "warning" | "info"),
            "severity is one of the three words, got {severity:?}"
        );
    }
}

// ── not a store: NOT_A_STORE issue, non-zero exit (not a bare open error) ────

#[test]
fn not_a_store_emits_issue_and_exits_nonzero() {
    // The committed `not-a-store/` sibling has no DB.md.
    let dir = corpus_b().join("not-a-store");
    let (code, json) = run_validate_json(&dir, false);
    assert_ne!(code, 0, "a non-store path fails validation");
    assert_eq!(
        code, 6,
        "reported as a validation issue (exit 6), not a bare open error"
    );
    let fired = codes(&json);
    assert_eq!(
        fired,
        BTreeSet::from(["NOT_A_STORE".to_string()]),
        "exactly one NOT_A_STORE issue"
    );
    assert_eq!(json["summary"]["errors"], 1);
}

// ── synthetic stores: byte-exact single-issue control ───────────────────────

#[test]
fn clean_synthetic_store_text_summary_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    // Two fully-valid content files in their canonical folders: a contact whose
    // `company` is a canonical full-path wiki-link to a company whose `domain`
    // it shares. Every field here is load-bearing for a clean working-set pass.
    write_file(
        tmp.path(),
        "records/contacts/sarah.md",
        "---\ntype: contact\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: a contact\nname: Sarah\nemail: sarah@acme.com\ncompany: \"[[records/companies/acme]]\"\n---\n\n# Sarah\n",
    );
    write_file(
        tmp.path(),
        "records/companies/acme.md",
        "---\ntype: company\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Acme\nname: Acme\ndomain: acme.com\n---\n\n# Acme\n",
    );
    // Log BOTH files as changed since the start of time so the working-set
    // default actually INSPECTS them — without a log entry the changed set is
    // empty and zero files are checked, which would make the clean assertion
    // below pass vacuously for any content. The working-set scope does NOT run
    // the `--all`-only index/log cross-file checks, so no index files are needed
    // for a truly-clean result here.
    write_file(
        tmp.path(),
        "log.md",
        "---\ntype: log\n---\n\n## [2026-05-22 10:00] update | records/contacts/sarah\nedited\n\n## [2026-05-22 10:00] update | records/companies/acme\nedited\n",
    );

    // The two logged-clean files are inspected and flag nothing.
    let out = dbmd().arg("validate").arg(tmp.path()).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "0 issue(s): 0 error(s), 0 warning(s), 0 info\n");
}

#[test]
fn working_set_default_inspects_logged_files() {
    // Regression guard for the test above: prove the working-set default truly
    // INSPECTS the logged file rather than passing vacuously on an empty changed
    // set. Same shape as `clean_synthetic_store_text_summary_only`, but the
    // logged contact carries a bad `created` timestamp — which MUST surface as a
    // single FM_BAD_TIMESTAMP error (exit 6), proving the seeded content is
    // load-bearing once the file is in the working set.
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    write_file(
        tmp.path(),
        "records/contacts/sarah.md",
        "---\ntype: contact\ncreated: NOT-A-TIMESTAMP\nupdated: 2026-05-01T00:00:00Z\nsummary: a contact\nname: Sarah\nemail: sarah@acme.com\n---\n\n# Sarah\n",
    );
    write_file(
        tmp.path(),
        "log.md",
        "---\ntype: log\n---\n\n## [2026-05-22 10:00] update | records/contacts/sarah\nedited\n",
    );

    let (code, json) = run_validate_json(tmp.path(), false);
    assert_eq!(code, 6, "a dirty logged file fails the working-set default");
    assert_eq!(json["scope"], "working-set");
    let bad_ts: Vec<&serde_json::Value> = json["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["code"] == "FM_BAD_TIMESTAMP")
        .collect();
    assert_eq!(
        bad_ts.len(),
        1,
        "one bad-timestamp error: {}",
        json["issues"]
    );
    assert_eq!(bad_ts[0]["file"], "records/contacts/sarah.md");
    assert_eq!(bad_ts[0]["severity"], "error");
}

#[test]
fn single_broken_link_is_one_error_exit_six() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    // A note linking to a nonexistent target — exactly one WIKI_LINK_BROKEN.
    write_file(
        tmp.path(),
        "records/notes/n.md",
        "---\ntype: note\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: a note\n---\n\nSee [[records/contacts/ghost]].\n",
    );

    let (code, json) = run_validate_json(tmp.path(), true);
    assert_eq!(code, 6);
    // Among the issues there must be exactly one WIKI_LINK_BROKEN on our file.
    let broken: Vec<&serde_json::Value> = json["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["code"] == "WIKI_LINK_BROKEN")
        .collect();
    assert_eq!(broken.len(), 1, "one broken link: {}", json["issues"]);
    assert_eq!(broken[0]["file"], "records/notes/n.md");
    assert_eq!(broken[0]["severity"], "error");
}

#[test]
fn since_flag_parses_date_only_and_rfc3339() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    // Two dirty logged files: `old` changed before the cutoff, `new` after.
    // Both `--since` forms must parse AND scope to the same cutoff — exactly
    // `new.md` is inspected (its FM_BAD_TIMESTAMP fires), `old.md` is excluded.
    // Asserting the scoped issue set, not bare exit 0 on an empty store, makes
    // the date load-bearing: a `--since` that's ignored or misparsed would
    // either inspect `old` too or inspect nothing, and this fails either way.
    write_file(
        tmp.path(),
        "records/contacts/old.md",
        "---\ntype: contact\ncreated: BAD-OLD\nupdated: 2026-05-01T00:00:00Z\nsummary: x\nname: A\n---\n\n# A\n",
    );
    write_file(
        tmp.path(),
        "records/contacts/new.md",
        "---\ntype: contact\ncreated: BAD-NEW\nupdated: 2026-05-01T00:00:00Z\nsummary: x\nname: B\n---\n\n# B\n",
    );
    write_file(
        tmp.path(),
        "log.md",
        concat!(
            "---\ntype: log\n---\n\n",
            "## [2026-04-20 10:00] update | records/contacts/old\nx\n\n",
            "## [2026-05-10 10:00] update | records/contacts/new\nx\n",
        ),
    );

    // The cutoff sits after `old`'s change and before `new`'s. Both spellings
    // resolve to it.
    for since in ["2026-05-01", "2026-05-01T00:00:00-07:00"] {
        let mut cmd = dbmd();
        cmd.arg("--json")
            .arg("validate")
            .arg("--since")
            .arg(since)
            .arg(tmp.path());
        let output = cmd.output().expect("run dbmd validate --since");
        assert_eq!(
            output.status.code(),
            Some(6),
            "--since {since}: the post-cutoff dirty file fails validation"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let files: BTreeSet<String> = json["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["file"].as_str().map(String::from))
            .collect();
        assert_eq!(
            files,
            BTreeSet::from(["records/contacts/new.md".to_string()]),
            "--since {since} scopes to only the post-cutoff file: {}",
            json["issues"]
        );
    }
}

#[test]
fn since_flag_rejects_garbage() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    dbmd()
        .arg("validate")
        .arg("--since")
        .arg("not-a-date")
        .arg(tmp.path())
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime (BAD_TIMESTAMP)
}
