//! Golden-master `insta` snapshots for the STABLE `--json` outputs.
//!
//! Closes the two `[~]` insta items in the toolkit plan (Block 2 line 399,
//! Block 5 line 484). The per-subcommand `assert_cmd` suites already assert the
//! *intent* of each command (exit codes, the path set returned, individual
//! record fields). These snapshots add the missing layer: they lock the exact
//! byte shape of the machine-facing `--json` envelope so a silent schema drift
//! (a renamed key, a reordered/added field, a formatting change, a regression
//! that emits spurious issues on a clean store) shows up as a reviewable diff
//! instead of slipping through.
//!
//! Scope discipline — only outputs that are deterministic AND carry real
//! regression value are snapshotted, all against the committed, read-only
//! `corpus-a-canonical`:
//!   - `dbmd search --json`        — the `file:line:text` match envelope
//!   - `dbmd validate --json`      — the `{scope, store, summary, issues}` report
//!   - `dbmd index query --json`   — full IndexRecords from the `.jsonl` sidecar
//!   - `dbmd fm query --json`      — same record shape via the frontmatter lens
//!   - `dbmd stats --json`         — the store-overview counts/sizes/types
//!   - `dbmd graph backlinks --json` — the incoming-link path array
//!
//! Determinism notes (why these are safe to freeze and the rest are not):
//!   - Every record/match path in the output is **store-relative** — no absolute
//!     path leaks into any of the five `--dir` commands (asserted out-of-band).
//!   - The `created`/`updated`/`last_touch` timestamps are *content* fields baked
//!     into the committed corpus files, not values minted at runtime — so they
//!     are part of the fixture and stay fixed across runs and machines.
//!   - JSON object key order is structurally stable: this build of `serde_json`
//!     has `preserve_order` off, so `json!`/Map serialization is `BTreeMap`
//!     (alphabetical) — not incidental run-to-run luck.
//!   - The one volatile field anywhere is `validate`'s `store`, which echoes the
//!     `[DIR]` arg verbatim. We neutralize it by running `validate` with the
//!     process CWD set to the corpus and `.` as the arg, so it serializes as the
//!     fixed string `"."` — no redaction/filter needed, and the command is a
//!     pure read (it writes nothing, asserted by the corpus-unchanged check in
//!     the matching `assert_cmd` suites).
//!
//! Deliberately NOT snapshotted: anything with runtime-minted timestamps
//! (`fm init`, `write`, `log` append), absolute-path-bearing human/text output,
//! or temp-store ordering — those have no stable golden and would be vacuous or
//! flaky.

mod common;

use common::{corpus_a, dbmd};

/// Run `dbmd <args> --json --dir corpus-a`, assert exit 0, return stdout as a
/// `String`. Used by the five commands that take the store via `--dir` and emit
/// only store-relative paths (no normalization needed).
fn json_stdout(args: &[&str]) -> String {
    let assert = dbmd()
        .arg("--json")
        .args(args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone())
        .expect("dbmd --json stdout is valid UTF-8")
}

// ── search ────────────────────────────────────────────────────────────────

/// `dbmd search "renewal" --in records --where meta-type=conclusion --json` —
/// the `file:line:text` match envelope over the conclusion-record candidate set
/// (the former wiki synthesis pages). Exercises the structured-filter → ripgrep
/// path end to end; every `file` is store-relative.
#[test]
fn search_json_envelope() {
    let out = json_stdout(&[
        "search",
        "renewal",
        "--in",
        "records",
        "--where",
        "meta-type=conclusion",
    ]);
    insta::assert_snapshot!("search_renewal_conclusion", out);
}

// ── validate ────────────────────────────────────────────────────────────────

/// `dbmd validate --all --json` on the canonical store: the SWEEP report on a
/// store that MUST stay clean. Locks the `{scope:"all", store, summary, issues}`
/// envelope and the zero-issue contract. Run with CWD=corpus + arg `.` so the
/// `store` field is the fixed `"."` (the only otherwise-volatile field).
#[test]
fn validate_all_clean_envelope() {
    let assert = dbmd()
        .current_dir(corpus_a())
        .arg("--json")
        .arg("validate")
        .arg("--all")
        .arg(".")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone())
        .expect("validate --json stdout is valid UTF-8");
    insta::assert_snapshot!("validate_all_clean", out);
}

/// `dbmd validate --json` (working-set scope) on the canonical store. Same
/// envelope as `--all` but the `scope` field flips to `"working-set"`, so this
/// locks the second code path's shape. Also clean on corpus-a.
#[test]
fn validate_working_set_clean_envelope() {
    let assert = dbmd()
        .current_dir(corpus_a())
        .arg("--json")
        .arg("validate")
        .arg(".")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone())
        .expect("validate --json stdout is valid UTF-8");
    insta::assert_snapshot!("validate_working_set_clean", out);
}

// ── query --type (structured read; --json = full records) ────────────────────

/// `dbmd query --type contact --json` — the full IndexRecords read out of the
/// `records/contacts/index.jsonl` sidecar, path-sorted. Locks the complete
/// record schema (path + summary + tags + links + every type-specific field).
#[test]
fn query_type_contact_json() {
    let out = json_stdout(&["query", "--type", "contact"]);
    insta::assert_snapshot!("query_type_contact", out);
}

// ── query --where (the dedup lens; former `fm query`) ────────────────────────

/// `dbmd query --where status=active --type contact --json` — the same sidecar
/// record shape reached through the frontmatter-filter lens. All four corpus
/// contacts are `status: active`, so this is the populated, path-sorted set.
#[test]
fn query_where_status_active_json() {
    let out = json_stdout(&["query", "--where", "status=active", "--type", "contact"]);
    insta::assert_snapshot!("query_where_status_active", out);
}

// ── stats ─────────────────────────────────────────────────────────────────

/// `dbmd stats --json` — the store overview: per-layer / per-type file counts,
/// total size, orphan + broken-link counts, recognized-vs-custom types, top
/// types. Every value is derived from the committed corpus content, so the whole
/// object is a fixed fixture. `stats` takes the store as a positional `[DIR]`.
#[test]
fn stats_json_overview() {
    let assert = dbmd()
        .arg("--json")
        .arg("stats")
        .arg(corpus_a())
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone())
        .expect("stats --json stdout is valid UTF-8");
    insta::assert_snapshot!("stats_overview", out);
}

// ── graph backlinks ─────────────────────────────────────────────────────────

/// `dbmd graph backlinks records/projects/northstar-renewal.md --json` — the
/// incoming wiki-link array (dependents / blast radius), store-relative and
/// sorted. The renewal project conclusion record is the most-linked hub.
#[test]
fn graph_backlinks_json() {
    let out = json_stdout(&[
        "graph",
        "backlinks",
        "records/projects/northstar-renewal.md",
    ]);
    insta::assert_snapshot!("graph_backlinks_northstar_renewal", out);
}
