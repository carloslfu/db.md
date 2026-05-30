//! End-to-end test: `dbmd log` month-rotation across the active/archive
//! boundary, driven through the real `dbmd` binary (not the library).
//!
//! What this pins, end to end (binary in, files + stdout out):
//!
//!   1. A store whose log spans multiple months and an append that crosses the
//!      month boundary: `dbmd log <kind> <object>` keeps the active `log.md` to
//!      the CURRENT month and rolls every strictly-earlier month into its own
//!      `log/<YYYY-MM>.md` archive (multiple older months → multiple archives).
//!
//!   2. `dbmd log tail N` reverse-reads correctly across the boundary: with N
//!      spanning active + archives it returns the whole timeline, oldest→newest,
//!      stitched from `log/<YYYY-MM>.md` archives behind the active file.
//!
//!   3. `dbmd log since <ts>` reverse-reads correctly across the boundary: a
//!      cutoff that lands inside an archived month returns exactly the strictly
//!      newer entries, crossing from the archives into the active file and
//!      excluding everything at/older than the cutoff.
//!
//! Determinism: the rotation trigger (`dbmd log <kind> <object>`) stamps
//! wall-clock "now", so the seeded "older" months use a fixed FAR-PAST year
//! (2023). That year is strictly before any plausible run date, so rotation
//! ALWAYS fires and never false-passes on the clock. To assert "the current
//! month stays active" without pulling chrono into this test crate, we read the
//! month back from the append's own `--json` `timestamp` — the binary's own
//! report of the instant it stamped. The read-path assertions (2) and (3) run
//! against a HAND-AUTHORED already-rotated store, so they are independent of the
//! wall clock entirely.

mod common;

use std::fs;
use std::path::Path;

use common::{dbmd, write_db_md};

// ── fixtures ────────────────────────────────────────────────────────────────

/// The `type: log` frontmatter every active/archive log file opens with — the
/// exact block `dbmd_core::log` writes, so a hand-authored fixture is
/// byte-compatible with one the tool produced.
const LOG_FRONTMATTER: &str = "---\ntype: log\n---\n\n# Curator log\n";

/// Build a fresh db.md store under a tempdir, returning `(guard, root)`. The
/// guard must stay in scope for the store to exist.
fn fresh_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path().join("store");
    fs::create_dir_all(&root).expect("create store root");
    write_db_md(&root);
    (tmp, root)
}

/// Render one log entry exactly as `dbmd_core::log` renders it on disk: the
/// `## [YYYY-MM-DD HH:MM] <kind> | <object>` header, the note body, then a
/// trailing blank line.
fn entry_block(ts: &str, kind: &str, object: &str, note: &str) -> String {
    format!("## [{ts}] {kind} | {object}\n{note}\n\n")
}

/// Render a STORE-WIDE entry exactly as the tool does: no object slot in the
/// header (`## [ts] <kind>`, no ` | object`). The append CLI's `-` sentinel is
/// an input convention; on disk the object is simply absent, so a faithful
/// fixture must omit the slot here too.
fn store_wide_block(ts: &str, kind: &str, note: &str) -> String {
    format!("## [{ts}] {kind}\n{note}\n\n")
}

/// Write an active `log.md` (frontmatter + the given entry blocks, in order).
fn write_active_log(root: &Path, blocks: &[String]) {
    let mut content = String::from(LOG_FRONTMATTER);
    content.push('\n');
    for b in blocks {
        content.push_str(b);
    }
    fs::write(root.join("log.md"), content).expect("write log.md");
}

/// Write a `log/<YYYY-MM>.md` archive (frontmatter + the given entry blocks).
fn write_archive(root: &Path, year_month: &str, blocks: &[String]) {
    let dir = root.join("log");
    fs::create_dir_all(&dir).expect("create log/ dir");
    let mut content = String::from(LOG_FRONTMATTER);
    content.push('\n');
    for b in blocks {
        content.push_str(b);
    }
    fs::write(dir.join(format!("{year_month}.md")), content).expect("write archive");
}

// ── helpers over the binary's output ──────────────────────────────────────────

/// Run `dbmd <args> --dir <root>` and return its stdout (asserting success).
fn run_read(root: &Path, args: &[&str]) -> String {
    let out = dbmd().args(args).arg("--dir").arg(root).assert().success();
    String::from_utf8(out.get_output().stdout.clone()).expect("utf8 stdout")
}

/// The ordered list of `(timestamp, kind, object)` header tuples in a text-mode
/// `dbmd log tail/since` output. The text renderer emits each entry's header as
/// `[YYYY-MM-DD HH:MM] <kind> | <object>` (or `[...] <kind>` for store-wide).
/// Parsing the headers back lets us assert order + membership without copying
/// the tool's prose.
fn header_tuples(stdout: &str) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        // A header line starts with `[` and has a `]` closing the timestamp.
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some(close) = rest.find(']') else {
            continue;
        };
        let ts = rest[..close].to_string();
        let after = rest[close + 1..].trim();
        if after.is_empty() {
            continue;
        }
        let (kind, object) = match after.split_once('|') {
            Some((k, o)) => (k.trim().to_string(), Some(o.trim().to_string())),
            None => (after.to_string(), None),
        };
        out.push((ts, kind, object));
    }
    out
}

// ── 1. rotation through the binary (multiple older months → archives) ─────────

#[test]
fn log_append_rotates_multiple_prior_months_into_archives_and_keeps_current_active() {
    let (_tmp, root) = fresh_store();

    // Seed an active log.md that spans TWO far-past months: 2023-10 and 2023-11.
    // (A real store accumulating writes over time looks exactly like this just
    // before the next month's first append.)
    let oct1 = entry_block("2023-10-05 09:00", "ingest", "sources/a", "october one");
    let oct2 = entry_block("2023-10-20 14:30", "create", "records/b", "october two");
    let nov1 = entry_block("2023-11-08 08:15", "update", "records/c", "november one");
    let nov2 = entry_block("2023-11-25 16:45", "link", "wiki/d", "november two");
    write_active_log(&root, &[oct1, oct2, nov1, nov2]);

    // No archive dir yet.
    assert!(
        !root.join("log").exists(),
        "precondition: no log/ archive dir before the boundary-crossing append"
    );

    // Append through the binary. The append form has no --dir, so it operates on
    // the current directory; it stamps wall-clock "now" (>= 2026), which is a
    // strictly later month than either seeded month — so BOTH 2023 months roll.
    let out = dbmd()
        .current_dir(&root)
        .args([
            "--json",
            "log",
            "update",
            "records/contacts/sarah-chen.md",
            "-m",
            "current-month entry",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let appended: serde_json::Value =
        serde_json::from_str(&stdout).expect("append --json is valid JSON");
    assert_eq!(appended["appended"], serde_json::json!(true));

    // The binary reports the exact instant it stamped; its YYYY-MM is the month
    // that must remain active. (Avoids pulling chrono into this test crate.)
    let stamped_ts = appended["timestamp"]
        .as_str()
        .expect("append --json carries a timestamp string");
    let current_ym = &stamped_ts[..7]; // "YYYY-MM"
    assert_ne!(
        current_ym, "2023-10",
        "sanity: the run clock is not the seeded far-past month"
    );
    assert_ne!(current_ym, "2023-11");

    // Both prior months rolled into their OWN archive file.
    let oct_archive = root.join("log").join("2023-10.md");
    let nov_archive = root.join("log").join("2023-11.md");
    assert!(
        oct_archive.exists(),
        "2023-10 entries must roll into log/2023-10.md"
    );
    assert!(
        nov_archive.exists(),
        "2023-11 entries must roll into log/2023-11.md"
    );

    // Each archive carries the log frontmatter and ONLY its own month's entries.
    let oct_text = fs::read_to_string(&oct_archive).unwrap();
    let nov_text = fs::read_to_string(&nov_archive).unwrap();
    assert!(oct_text.starts_with("---\ntype: log\n---\n"));
    assert!(nov_text.starts_with("---\ntype: log\n---\n"));
    assert!(oct_text.contains("## [2023-10-05 09:00] ingest | sources/a"));
    assert!(oct_text.contains("## [2023-10-20 14:30] create | records/b"));
    assert!(
        !oct_text.contains("november"),
        "October archive must not hold November entries:\n{oct_text}"
    );
    assert!(nov_text.contains("## [2023-11-08 08:15] update | records/c"));
    assert!(nov_text.contains("## [2023-11-25 16:45] link | wiki/d"));
    assert!(
        !nov_text.contains("october"),
        "November archive must not hold October entries:\n{nov_text}"
    );

    // The active log.md now holds ONLY the current-month entry: no 2023 entries
    // survive, and its single header carries the stamped current month.
    let active = fs::read_to_string(root.join("log.md")).unwrap();
    assert!(
        !active.contains("2023-10") && !active.contains("2023-11"),
        "no prior-month entries may remain in the active file:\n{active}"
    );
    assert!(
        active.contains("records/contacts/sarah-chen.md"),
        "the appended entry stays in the active file:\n{active}"
    );
    let active_headers = header_tuples_in_raw(&active);
    assert_eq!(
        active_headers.len(),
        1,
        "exactly one entry (the current-month append) stays active:\n{active}"
    );
    assert!(
        active_headers[0].starts_with(current_ym),
        "the active entry is in the current month {current_ym}; got {:?}",
        active_headers[0]
    );

    // End to end: the full timeline read back through the binary is intact and
    // chronological across both archives + the active file.
    let tail_out = run_read(&root, &["log", "tail", "10"]);
    let tuples = header_tuples(&tail_out);
    let kinds: Vec<&str> = tuples.iter().map(|(_, k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["ingest", "create", "update", "link", "update"],
        "tail across archives+active is the whole timeline, oldest→newest:\n{tail_out}"
    );
    // The first four timestamps are the seeded 2023 ones in order; the last is
    // the current-month append.
    assert_eq!(tuples[0].0, "2023-10-05 09:00");
    assert_eq!(tuples[1].0, "2023-10-20 14:30");
    assert_eq!(tuples[2].0, "2023-11-08 08:15");
    assert_eq!(tuples[3].0, "2023-11-25 16:45");
    assert!(tuples[4].0.starts_with(current_ym));
}

/// Header timestamps (`YYYY-MM-DD HH:MM`) from a raw on-disk log file's
/// `## [...]` headers, in file order. Used to assert what stayed in the active
/// file after rotation.
fn header_tuples_in_raw(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("## [")?;
            let close = rest.find(']')?;
            Some(rest[..close].to_string())
        })
        .collect()
}

// ── 2. tail reverse-reads across the boundary (hand-authored rotated store) ────

#[test]
fn log_tail_reverse_reads_across_active_and_archive_boundary() {
    // A frozen, already-rotated store: active log.md is one (recent) month, with
    // two older months sitting in log/<YYYY-MM>.md archives. Fully deterministic
    // — no wall-clock involved in the read path.
    let (_tmp, root) = fresh_store();

    write_archive(
        &root,
        "2023-11",
        &[
            entry_block("2023-11-08 08:15", "ingest", "sources/n1", "nov one"),
            entry_block("2023-11-25 16:45", "create", "records/n2", "nov two"),
        ],
    );
    write_archive(
        &root,
        "2023-12",
        &[
            entry_block("2023-12-03 10:00", "update", "records/d1", "dec one"),
            entry_block("2023-12-30 23:10", "link", "wiki/d2", "dec two"),
        ],
    );
    write_active_log(
        &root,
        &[
            entry_block("2024-01-04 09:30", "update", "records/j1", "jan one"),
            store_wide_block("2024-01-19 12:00", "validate", "jan validate"),
        ],
    );

    // tail 2 stays inside the active month and never needs an archive.
    let t2 = header_tuples(&run_read(&root, &["log", "tail", "2"]));
    assert_eq!(
        t2.iter().map(|(ts, _, _)| ts.as_str()).collect::<Vec<_>>(),
        vec!["2024-01-04 09:30", "2024-01-19 12:00"],
        "tail 2 = the two newest (active month), chronological"
    );

    // tail 3 must cross ONE boundary: reach back into the 2023-12 archive for
    // the third-newest entry.
    let t3 = header_tuples(&run_read(&root, &["log", "tail", "3"]));
    assert_eq!(
        t3.iter().map(|(ts, _, _)| ts.as_str()).collect::<Vec<_>>(),
        vec!["2023-12-30 23:10", "2024-01-04 09:30", "2024-01-19 12:00"],
        "tail 3 crosses into the 2023-12 archive for the 3rd-newest"
    );

    // tail 6 must cross BOTH boundaries: the full timeline, archives + active,
    // oldest→newest.
    let t6 = header_tuples(&run_read(&root, &["log", "tail", "6"]));
    assert_eq!(
        t6.iter().map(|(ts, _, _)| ts.as_str()).collect::<Vec<_>>(),
        vec![
            "2023-11-08 08:15",
            "2023-11-25 16:45",
            "2023-12-03 10:00",
            "2023-12-30 23:10",
            "2024-01-04 09:30",
            "2024-01-19 12:00",
        ],
        "tail 6 stitches both archives behind the active file, in order"
    );

    // tail larger than the whole log returns everything (no over-read, no dup).
    let t_all = header_tuples(&run_read(&root, &["log", "tail", "999"]));
    assert_eq!(
        t_all.len(),
        6,
        "tail 999 returns the 6 real entries, no more"
    );
    assert_eq!(
        t_all, t6,
        "over-large tail equals the full ordered timeline"
    );

    // The store-wide `validate` entry round-trips with no object slot through the
    // binary (its header has no ` | object`).
    let last = t_all.last().unwrap();
    assert_eq!(last.1, "validate");
    assert_eq!(
        last.2, None,
        "store-wide validate header has no object slot"
    );
}

// ── 3. since reverse-reads across the boundary and early-stops ─────────────────

#[test]
fn log_since_reverse_reads_across_boundary_and_excludes_cutoff() {
    // Same frozen rotated store shape as the tail test.
    let (_tmp, root) = fresh_store();

    write_archive(
        &root,
        "2023-11",
        &[
            entry_block("2023-11-08 08:15", "ingest", "sources/n1", "nov one"),
            entry_block("2023-11-25 16:45", "create", "records/n2", "nov two"),
        ],
    );
    write_archive(
        &root,
        "2023-12",
        &[
            entry_block("2023-12-03 10:00", "update", "records/d1", "dec one"),
            entry_block("2023-12-30 23:10", "link", "wiki/d2", "dec two"),
        ],
    );
    write_active_log(
        &root,
        &[entry_block(
            "2024-01-04 09:30",
            "update",
            "records/j1",
            "jan one",
        )],
    );

    // since a mid-archive instant (2023-12-03 10:00, exactly the dec-one entry):
    // strictly-newer means dec-one is EXCLUDED; dec-two (archive) and jan-one
    // (active) are returned, crossing the archive→active boundary.
    let s = header_tuples(&run_read(&root, &["log", "since", "2023-12-03T10:00:00Z"]));
    assert_eq!(
        s.iter().map(|(ts, _, _)| ts.as_str()).collect::<Vec<_>>(),
        vec!["2023-12-30 23:10", "2024-01-04 09:30"],
        "since is exclusive of the exact cutoff and crosses archive→active"
    );

    // since a cutoff inside the OLDER archive month: pulls the later 2023-11
    // entry, BOTH 2023-12 entries, and the active 2024-01 entry — three
    // boundaries' worth, still ordered, still excluding the earlier 2023-11 one.
    let s2 = header_tuples(&run_read(&root, &["log", "since", "2023-11-08T08:15:00Z"]));
    assert_eq!(
        s2.iter().map(|(ts, _, _)| ts.as_str()).collect::<Vec<_>>(),
        vec![
            "2023-11-25 16:45",
            "2023-12-03 10:00",
            "2023-12-30 23:10",
            "2024-01-04 09:30",
        ],
        "since deep in the oldest archive returns all strictly-newer, in order"
    );

    // since AFTER everything in the store returns nothing (early stop in active).
    let s_none = run_read(&root, &["log", "since", "2024-02-01T00:00:00Z"]);
    assert!(
        header_tuples(&s_none).is_empty(),
        "since after the newest entry returns no entries:\n{s_none}"
    );

    // since BEFORE everything returns the whole timeline across both archives.
    let s_all = header_tuples(&run_read(&root, &["log", "since", "2023-01-01T00:00:00Z"]));
    assert_eq!(
        s_all
            .iter()
            .map(|(ts, _, _)| ts.as_str())
            .collect::<Vec<_>>(),
        vec![
            "2023-11-08 08:15",
            "2023-11-25 16:45",
            "2023-12-03 10:00",
            "2023-12-30 23:10",
            "2024-01-04 09:30",
        ],
        "since before the oldest entry returns the full ordered timeline"
    );
}

// ── 4. since on an out-of-order (append-only correction) active log ────────────

#[test]
fn log_since_handles_non_monotonic_active_log() {
    // The append-only SPEC permits a backdated CORRECTIVE entry below the entry
    // it corrects (out-of-order is only the LOG_OUT_OF_ORDER warning, never
    // rejected; a merge=union clone merge interleaves the same way). Author a
    // log.md whose physical order is 10:10, 10:05, 10:00 — the backdated 10:00
    // correction sits LAST. `dbmd log since 10:02` must still surface the two
    // newer entries (10:05, 10:10); a within-file early stop would hit the
    // physically-last 10:00 entry and return EMPTY.
    let (_tmp, root) = fresh_store();
    write_active_log(
        &root,
        &[
            entry_block("2026-05-27 10:10", "update", "records/c", "newest"),
            entry_block("2026-05-27 10:05", "create", "records/b", "middle"),
            entry_block("2026-05-27 10:00", "update", "records/a", "backdated fix"),
        ],
    );

    let s = header_tuples(&run_read(&root, &["log", "since", "2026-05-27T10:02:00Z"]));
    let mut stamps: Vec<&str> = s.iter().map(|(ts, _, _)| ts.as_str()).collect();
    stamps.sort_unstable();
    assert_eq!(
        stamps,
        vec!["2026-05-27 10:05", "2026-05-27 10:10"],
        "since(10:02) over a non-monotonic log must return both newer entries, \
         not stop at the physically-last backdated 10:00 entry:\n{s:?}"
    );

    // A cutoff before everything still returns all three regardless of disk order.
    let s_all = header_tuples(&run_read(&root, &["log", "since", "2026-05-27T09:00:00Z"]));
    assert_eq!(
        s_all.len(),
        3,
        "since before everything returns all 3 entries"
    );
}
