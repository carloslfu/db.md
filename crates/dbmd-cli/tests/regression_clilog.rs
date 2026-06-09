//! Regression tests for confirmed launch-readiness findings on `dbmd log`.
//!
//! Each test reconstructs a finding's exact trigger and asserts the corrected
//! behavior, so the specific bug can never silently return:
//!   - #23 — a trailing global `--json` / `--color` on the append form was
//!     captured as a third positional by the `external_subcommand` and rejected
//!     with "too many arguments" (exit 1, no append). The flag now applies.
//!   - #24 — `dbmd log tail --help` advertised "newest first" while the output
//!     is oldest→newest (chronological). The help text now matches the behavior.

mod common;

use common::{copy_store_to_temp, corpus_a, dbmd};

// ── #23: trailing global flags on the append form ───────────────────────────────

/// `dbmd log create <obj> -m <note> --json` (the `--json` placed LAST, the
/// natural trailing-flag habit every other subcommand accepts) must succeed,
/// emit the append JSON report, AND actually append the entry — not fail with
/// "too many arguments".
///
/// Pre-fix: clap's `external_subcommand` captured `--json` verbatim as a third
/// positional, `ParsedAppend::from_tokens` saw `positionals.len() == 3`, and the
/// command exited 1 with `LOG_USAGE` "too many arguments" before any append.
#[test]
fn regression_append_accepts_trailing_json_flag() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    let out = dbmd()
        .current_dir(&store)
        .args([
            "log",
            "create",
            "records/contacts/sarah-chen.md",
            "-m",
            "weekly sync",
            "--json",
        ])
        .assert()
        .success();

    // The trailing `--json` must take effect: stdout is the append JSON report,
    // not the human header line.
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("trailing --json yields a JSON report");
    assert_eq!(v["appended"], serde_json::json!(true));
    assert_eq!(v["kind"], serde_json::json!("create"));
    assert_eq!(
        v["object"],
        serde_json::json!("records/contacts/sarah-chen.md")
    );

    // And the entry actually landed in the active log (the append was performed,
    // not rejected before the write).
    let log = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert!(
        log.contains("create | records/contacts/sarah-chen.md"),
        "the appended header landed in log.md:\n{log}"
    );
    assert!(
        log.contains("weekly sync"),
        "the note landed in log.md:\n{log}"
    );
}

/// The exact same invocation with `--json` placed FIRST (the form the existing
/// suite uses) already worked; pin that the two placements now produce the same
/// JSON contract so the trailing form is a true equivalent, not a near-miss.
#[test]
fn regression_append_trailing_json_matches_leading_json() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    let leading = dbmd()
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
    let leading_out = String::from_utf8(leading.get_output().stdout.clone()).unwrap();
    let leading_v: serde_json::Value = serde_json::from_str(&leading_out).unwrap();

    let trailing = dbmd()
        .current_dir(&store)
        .args([
            "log",
            "update",
            "records/contacts/david-kim.md",
            "-m",
            "x",
            "--json",
        ])
        .assert()
        .success();
    let trailing_out = String::from_utf8(trailing.get_output().stdout.clone()).unwrap();
    let trailing_v: serde_json::Value = serde_json::from_str(&trailing_out).unwrap();

    // Timestamps differ by wall-clock; the structural keys must match.
    assert_eq!(leading_v["appended"], trailing_v["appended"]);
    assert_eq!(leading_v["kind"], trailing_v["kind"]);
    assert_eq!(leading_v["object"], trailing_v["object"]);
}

/// A trailing `--color never` (the value-taking global flag) on the append form
/// must also be stripped, not counted as a positional. Pre-fix it produced two
/// extra positionals (`--color` and `never`) → "too many arguments", exit 1.
#[test]
fn regression_append_accepts_trailing_color_flag() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let before = std::fs::read_to_string(store.join("log.md")).unwrap();

    let out = dbmd()
        .current_dir(&store)
        .args([
            "log",
            "create",
            "records/contacts/sarah-chen.md",
            "-m",
            "color test",
            "--color",
            "never",
        ])
        .assert()
        .success();

    // Human (non-JSON) output: the canonical header line is still echoed, and the
    // append performed.
    let echoed = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        echoed.contains("create | records/contacts/sarah-chen.md"),
        "append still echoes its header under --color never:\n{echoed}"
    );
    let after = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert_ne!(before, after, "the append landed in log.md");
}

/// An unrecognized trailing positional (a genuine third positional, not a global
/// flag) must STILL be rejected — the fix strips only the known globals and must
/// not swallow real arity errors.
#[test]
fn regression_append_still_rejects_a_real_third_positional() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let before = std::fs::read_to_string(store.join("log.md")).unwrap();

    dbmd()
        .current_dir(&store)
        .args([
            "log",
            "create",
            "records/contacts/sarah-chen.md",
            "an-unquoted-extra-token",
        ])
        .assert()
        .failure()
        .code(1);

    let after = std::fs::read_to_string(store.join("log.md")).unwrap();
    assert_eq!(
        before, after,
        "a rejected append (real extra positional) must not mutate log.md"
    );
}

// ── #24: `log tail --help` order claim matches the emitted order ─────────────────

/// `dbmd log tail --help` must NOT advertise "newest first" — the emitted order
/// is oldest→newest (chronological), pinned by
/// `tail_default_returns_newest_entries_chronologically` and core `into_sorted`.
/// Pre-fix the clap `///` help said "newest first", contradicting the output and
/// misleading an agent into reading entry[0] as the most recent.
#[test]
fn regression_tail_help_describes_chronological_order() {
    let out = dbmd().args(["log", "tail", "--help"]).assert().success();
    let help = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lower = help.to_lowercase();

    assert!(
        !lower.contains("newest first"),
        "tail help must not claim 'newest first' (output is oldest→newest):\n{help}"
    );
    assert!(
        lower.contains("oldest") && lower.contains("newest"),
        "tail help should describe the oldest→newest order explicitly:\n{help}"
    );
}
