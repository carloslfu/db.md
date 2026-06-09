//! Regression tests for `dbmd search` — confirmed launch-readiness findings.
//!
//! Finding #7 (high): structured search (`--type` / `--where`) aborted the whole
//! command with a fatal `SEARCH_FAILED` whenever the type-folder `index.jsonl`
//! sidecar still listed a file that had been removed from disk out-of-band (e.g.
//! `rm` / `git checkout` of an older branch) without a re-`index`. The candidate
//! set comes verbatim from the sidecar with no existence check, so `search_path`
//! hit `File::open(...) -> NotFound` on the stale entry and discarded every match
//! found so far — while the all-content walk and the link path both tolerate a
//! missing file. These lock in the corrected behavior: a stale sidecar entry is
//! skipped, and the matches from the files that DO exist are still returned.

mod common;

use std::collections::BTreeSet;

use common::{copy_store_to_temp, corpus_a, dbmd};

/// Parse `dbmd search --json` stdout into the deduped set of matched files.
fn matched_files(stdout: &[u8]) -> BTreeSet<String> {
    let stdout = String::from_utf8(stdout.to_vec()).expect("search --json is utf8");
    let matches: serde_json::Value =
        serde_json::from_str(&stdout).expect("search --json is a JSON array");
    matches
        .as_array()
        .expect("search --json is an array")
        .iter()
        .map(|m| {
            m["file"]
                .as_str()
                .expect("each match carries a file")
                .to_string()
        })
        .collect()
}

#[test]
fn regression_structured_search_skips_stale_sidecar_entry() {
    // Trigger (verbatim from the finding): corpus-a's committed
    // `records/contacts/index.jsonl` already lists all four contacts. Delete one
    // contact file out-of-band WITHOUT re-running `dbmd index`, so the sidecar is
    // now stale (it still names `records/contacts/sarah-chen.md`).
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let stale = store.join("records/contacts/sarah-chen.md");
    assert!(stale.is_file(), "the contact must exist before deletion");
    std::fs::remove_file(&stale).expect("remove the contact out-of-band");

    // `Northstar` appears in all four contact records, so the sidecar yields the
    // deleted file as a candidate. Pre-fix, `search_path` on the missing file
    // raised a fatal SEARCH_FAILED (exit 1, no output); post-fix the search
    // succeeds and returns the matches from the three contacts that still exist.
    let out = dbmd()
        .arg("--json")
        .arg("search")
        .arg("Northstar")
        .arg("--type")
        .arg("contact")
        .arg("--dir")
        .arg(&store)
        .assert()
        .success();

    let files = matched_files(&out.get_output().stdout);
    let expected: BTreeSet<String> = [
        "records/contacts/david-kim.md",
        "records/contacts/elena-rodriguez.md",
        "records/contacts/marcus-okafor.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        files, expected,
        "a stale sidecar entry must be skipped, not abort the search; \
         the three surviving contacts must still match"
    );
    assert!(
        !files.contains("records/contacts/sarah-chen.md"),
        "the deleted contact must not appear in the results: {files:?}"
    );
}

#[test]
fn regression_structured_search_all_candidates_missing_is_empty_success() {
    // The degenerate case: every structured candidate has been deleted. The old
    // code aborted on the first NotFound; the fixed code skips them all and
    // returns an empty result with exit 0 ("not found" is data, not an error) —
    // never a SEARCH_FAILED.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    for name in [
        "sarah-chen.md",
        "elena-rodriguez.md",
        "marcus-okafor.md",
        "david-kim.md",
    ] {
        let f = store.join("records/contacts").join(name);
        std::fs::remove_file(&f).expect("remove contact out-of-band");
    }

    let out = dbmd()
        .arg("--json")
        .arg("search")
        .arg("Northstar")
        .arg("--type")
        .arg("contact")
        .arg("--dir")
        .arg(&store)
        .assert()
        .success();

    let files = matched_files(&out.get_output().stdout);
    assert!(
        files.is_empty(),
        "all candidates deleted → empty success, not a fatal error: {files:?}"
    );
}
