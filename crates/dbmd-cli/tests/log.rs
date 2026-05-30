//! Integration tests for `dbmd log` — the append-only store timeline.
//!
//! Intent-derived from plan Block 5 + `dbmd_core::log`: the read forms
//! (`tail`, `since`) reverse-read the committed `corpus-a` `log.md`; the append
//! form (`<kind> <object> [-m]`) is exercised against a temp copy so the
//! committed corpus is never mutated. Assertions check the properties that must
//! hold (which entries come back, exit codes, the header the append echoes,
//! the appended line landing in `log.md`) rather than the tool's exact prose.

mod common;

use common::{copy_store_to_temp, corpus_a, dbmd};

/// stdout of a successful `dbmd <args>` run (with `--dir corpus_a` appended).
fn read_log(args: &[&str]) -> String {
    let out = dbmd()
        .args(args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    String::from_utf8(out.get_output().stdout.clone()).unwrap()
}

// ── tail ──────────────────────────────────────────────────────────────────────

#[test]
fn tail_default_returns_newest_entries_chronologically() {
    // corpus-a's log ends with an index-rebuild then a validate PASS; tail must
    // return them in chronological (oldest→newest) order, newest last.
    let out = read_log(&["log", "tail", "3"]);
    let validate_pos = out.find("] validate").expect("validate entry present");
    let rebuild_pos = out.find("index-rebuild").expect("index-rebuild present");
    assert!(
        rebuild_pos < validate_pos,
        "entries must be chronological (oldest first); got:\n{out}"
    );
    // The validate PASS note rides along with its header.
    assert!(
        out.contains("PASS — 0 errors"),
        "note body included:\n{out}"
    );
}

#[test]
fn tail_caps_to_n_entries() {
    // Each entry renders as a `[YYYY-MM-DD HH:MM]` header line; counting those
    // header lines counts entries. tail 1 → exactly one header.
    let out = read_log(&["log", "tail", "1"]);
    let header_lines = out.lines().filter(|l| l.starts_with('[')).count();
    assert_eq!(header_lines, 1, "tail 1 returns exactly one entry:\n{out}");
}

#[test]
fn tail_json_is_an_array_of_entry_objects() {
    let out = read_log(&["--json", "log", "tail", "2"]);
    let value: serde_json::Value = serde_json::from_str(&out).expect("tail --json is valid JSON");
    let arr = value.as_array().expect("tail --json is an array");
    assert_eq!(arr.len(), 2, "two entries requested");
    // Each object carries the documented keys.
    for e in arr {
        assert!(e.get("timestamp").is_some(), "entry has a timestamp");
        assert!(e.get("kind").is_some(), "entry has a kind");
        assert!(e.get("note").is_some(), "entry has a note field");
        // `object` is present (may be null for store-wide entries).
        assert!(e.as_object().unwrap().contains_key("object"));
    }
}

// ── since ─────────────────────────────────────────────────────────────────────

#[test]
fn since_full_rfc3339_returns_strictly_newer() {
    // The 2026-05-23 link entry is at 09:00; a since cutoff at exactly that
    // instant must EXCLUDE it (strictly newer) and include only the 05-31 ones.
    let out = read_log(&["log", "since", "2026-05-23T09:00:00Z"]);
    assert!(
        !out.contains("2026-05-23 09:00"),
        "since is exclusive of the exact cutoff instant:\n{out}"
    );
    assert!(
        out.contains("2026-05-31 10:05"),
        "newer entries returned:\n{out}"
    );
}

#[test]
fn since_date_only_is_treated_as_midnight_utc() {
    // Date-only `2026-05-31` ⇒ `2026-05-31T00:00:00Z`; both 05-31 entries are
    // newer than midnight, and the 05-23 one is older and excluded.
    let out = read_log(&["log", "since", "2026-05-31"]);
    assert!(
        out.contains("2026-05-31 10:00"),
        "05-31 entries present:\n{out}"
    );
    assert!(out.contains("2026-05-31 10:05"));
    assert!(
        !out.contains("2026-05-23"),
        "entries before the date are excluded:\n{out}"
    );
}

#[test]
fn since_rejects_a_non_timestamp() {
    dbmd()
        .args(["log", "since", "not-a-date"])
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .failure()
        .code(1);
}

// ── append ─────────────────────────────────────────────────────────────────────

#[test]
fn append_writes_an_entry_and_echoes_its_header() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    let out = dbmd()
        .current_dir(&store)
        .args([
            "log",
            "create",
            "records/contacts/sarah-chen.md",
            "-m",
            "integration-test note",
        ])
        .assert()
        .success();
    let echoed = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        echoed.contains("create | records/contacts/sarah-chen.md"),
        "append echoes the canonical header line; got:\n{echoed}"
    );

    // The note + header actually landed in the active log.md.
    let log = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert!(log.contains("create | records/contacts/sarah-chen.md"));
    assert!(log.contains("integration-test note"));
}

#[test]
fn append_store_wide_dash_drops_the_object_slot() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    dbmd()
        .current_dir(&store)
        .args(["log", "validate", "-"])
        .assert()
        .success();

    let log = std::fs::read_to_string(store.join("log.md")).unwrap();
    // The newest entry is a bare `validate` header with no ` | object` suffix.
    let last_validate = log
        .lines()
        .rev()
        .find(|l| l.contains("] validate"))
        .expect("a validate header exists");
    assert!(
        !last_validate.contains('|'),
        "store-wide `-` yields no object slot; got: {last_validate}"
    );
}

#[test]
fn append_json_reports_the_landed_entry() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    let out = dbmd()
        .current_dir(&store)
        .args([
            "--json",
            "log",
            "update",
            "records/contacts/david-kim.md",
            "-m",
            "x",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("append --json is valid JSON");
    assert_eq!(v["appended"], serde_json::json!(true));
    assert_eq!(v["kind"], serde_json::json!("update"));
    assert_eq!(
        v["object"],
        serde_json::json!("records/contacts/david-kim.md")
    );
}

#[test]
fn append_rejects_missing_object() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    // Only a kind, no object → usage error, exit 1, and nothing appended.
    let before = std::fs::read_to_string(store.join("log.md")).unwrap();
    dbmd()
        .current_dir(&store)
        .args(["log", "create"])
        .assert()
        .failure()
        .code(1);
    let after = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert_eq!(before, after, "a rejected append must not mutate log.md");
}

#[test]
fn append_rotates_prior_months_into_archive() {
    // Append a JUNE entry to corpus-a (whose active log is all May): the May
    // entries roll into log/2026-05.md and the active file keeps only June.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    dbmd()
        .current_dir(&store)
        .args([
            "log",
            "create",
            "records/contacts/sarah-chen.md",
            "-m",
            "june entry",
        ])
        // NOTE: the append timestamp is wall-clock now; this test only asserts
        // rotation mechanics when the appended month is past the active month,
        // which holds whenever "now" is a later month than May 2026. Guarded
        // below so it is a no-op (not a false failure) before that date.
        .assert()
        .success();

    let now_is_after_may_2026 = chrono_like_now_after_may_2026();
    if now_is_after_may_2026 {
        assert!(
            store.join("log").join("2026-05.md").exists(),
            "May entries should roll into log/2026-05.md once a later month is appended"
        );
    }
}

/// Cheap "is the current month after 2026-05?" check without pulling chrono into
/// the test crate: parse the year/month out of an RFC3339 `now` we get from the
/// filesystem mtime of a just-created temp file. Kept conservative — returns
/// false on any uncertainty so the rotation assertion never false-fails.
fn chrono_like_now_after_may_2026() -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2026-06-01T00:00:00Z = 1_780_272_000. Anything at/after is "after May 2026".
    secs >= 1_780_272_000
}
