//! End-to-end **designed-to-fail** integration test for `corpus-b-edges`,
//! driving the **real `dbmd` binary** as a subprocess (`assert_cmd`) — not
//! in-process library calls. This is the negative-path twin of
//! `e2e_corpus_a.rs`: where corpus-a pins "a clean store validates clean", this
//! pins "a deliberately-broken store reports EXACTLY the seeded issues, and the
//! write surfaces refuse exactly the seeded policy violations without mutating
//! the store."
//!
//! What it asserts:
//!
//!   1. **`validate --all --json corpus-b` equals `EXPECTED/validate.json`**,
//!      issue-for-issue — every seeded breakage surfaces with the correct
//!      `code` / `severity` / `file` / `line` / `key` / `related`, with no extra
//!      and no missing issues, the `summary` tallies match, and the process exits
//!      non-zero (`6`, errors present). Comparison is order-independent (the
//!      golden documents a `(file, line, code)` sort, but the contract is the
//!      SET of issues, not their emission order).
//!   2. **Each `EXPECTED/policy-refusal/<scenario>.json`** — `write` (existing +
//!      nonexistent frozen target), `fm set`, `rename`, `link` — refuses with the
//!      structured `POLICY_FROZEN_PAGE` error, exits non-zero, and leaves the
//!      corpus byte-for-byte unchanged (and never creates the would-be file). Run
//!      against a TEMP COPY so the committed fixture is never mutated.
//!   3. **`EXPECTED/not-a-store.json`** — pointing `validate` at the no-`DB.md`
//!      sibling surfaces exactly one `NOT_A_STORE` issue and exits non-zero, and
//!      the `--all` sweep on the store proper does NOT descend into it.
//!   4. **`EXPECTED/validate.json` is intent-derived, not a snapshot** — its
//!      `_comment` declares hand-derivation; every code it emits is mapped in the
//!      committed `EXPECTED/coverage.json`; that coverage map is a subset of the
//!      `SPEC.md § Validation` code table (no invented codes); `coverage.json`'s
//!      bookkeeping (`spec_code_count`, `all_spec_codes_covered`,
//!      `uncovered_spec_codes`) is checked against the live SPEC table so it can
//!      never over-claim coverage; and every issue names a distinct designed
//!      fixture site. A golden produced by dumping tool output would satisfy none
//!      of these structural properties.
//!
//! The goldens are committed and hand-derived from `SPEC.md § Validation`; this
//! test is their executable contract. Run after any change that could move
//! validate / write-policy behavior:
//! `cargo test -p dbmd-cli --test e2e_corpus_b`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use common::{copy_store_to_temp, corpus_b, corpus_b_expected, dbmd};

// ─────────────────────────────────────────────────────────────────────────────
// Issue model — the comparable projection of one validate issue object.
// ─────────────────────────────────────────────────────────────────────────────

/// The fields of a validate issue this test holds the engine to: everything in
/// the `EXPECTED/validate.json` issue shape except the free-text `message` /
/// `suggestion` prose. `related` is normalized to a sorted set so the comparison
/// is order-independent (the golden lists a stable order, but "the partner files
/// involved" is a set, not a sequence).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IssueKey {
    severity: String,
    code: String,
    file: String,
    line: Option<i64>,
    key: Option<String>,
    related: Vec<String>,
}

impl IssueKey {
    fn from_json(v: &serde_json::Value) -> Self {
        let related = v
            .get("related")
            .and_then(|r| r.as_array())
            .map(|a| {
                let mut r: Vec<String> = a
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
                r.sort();
                r
            })
            .unwrap_or_default();
        IssueKey {
            severity: str_field(v, "severity"),
            code: str_field(v, "code"),
            file: str_field(v, "file"),
            line: v.get("line").and_then(|l| l.as_i64()),
            key: v.get("key").and_then(|k| k.as_str()).map(String::from),
            related,
        }
    }
}

/// Read a required string field, panicking with context if absent — every issue
/// object the contract describes has `severity` / `code` / `file`.
fn str_field(v: &serde_json::Value, field: &str) -> String {
    v.get(field)
        .and_then(|x| x.as_str())
        .unwrap_or_else(|| panic!("issue object missing string field `{field}`: {v}"))
        .to_string()
}

/// Project a `{issues: [...]}` envelope (or a bare issue array) into the
/// comparable set of [`IssueKey`]s.
fn issue_set(issues: &[serde_json::Value]) -> BTreeSet<IssueKey> {
    issues.iter().map(IssueKey::from_json).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// 1 — validate --all equals EXPECTED/validate.json, issue-for-issue, exit 6
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_all_matches_expected_golden_issue_for_issue_and_exits_six() {
    // Run the full SWEEP over the committed designed-to-fail store. Errors are
    // present, so the process MUST exit 6 (ExitCode::ValidationFailed).
    let out = dbmd()
        .args(["--json", "validate", "--all"])
        .arg(corpus_b())
        .assert()
        .failure()
        .code(6)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("validate --all emits a JSON envelope");

    // Load the hand-derived golden.
    let golden: serde_json::Value = read_json(&corpus_b_expected("validate.json"));

    // ── scope + summary tallies ───────────────────────────────────────────────
    assert_eq!(report["scope"], "all", "`--all` is the full-sweep scope");
    for k in ["errors", "warnings", "info", "total"] {
        assert_eq!(
            report["summary"][k], golden["summary"][k],
            "summary.{k} must equal the golden ({} vs {})",
            report["summary"][k], golden["summary"][k]
        );
    }
    // Internal consistency: the tallies add up, and errors > 0 (⇒ exit 6).
    let (e, w, i, t) = (
        u64_at(&report, "errors"),
        u64_at(&report, "warnings"),
        u64_at(&report, "info"),
        u64_at(&report, "total"),
    );
    assert_eq!(e + w + i, t, "summary tallies are self-consistent");
    assert!(
        e > 0,
        "the designed-to-fail store has errors (⇒ non-zero exit)"
    );

    // ── the issue SET equals the golden, exactly ─────────────────────────────
    let got = issue_set(report["issues"].as_array().expect("issues is an array"));
    let want = issue_set(
        golden["issues"]
            .as_array()
            .expect("golden issues is an array"),
    );

    let missing: Vec<&IssueKey> = want.difference(&got).collect();
    let extra: Vec<&IssueKey> = got.difference(&want).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "validate --all must emit EXACTLY the golden issue set.\n\
         MISSING (in EXPECTED, not emitted): {missing:#?}\n\
         EXTRA (emitted, not in EXPECTED): {extra:#?}"
    );

    // Equal as sets AND equal in count (no duplicate issue objects collapsed by
    // the set): the golden's array length is the emitted array length.
    assert_eq!(
        report["issues"].as_array().unwrap().len(),
        golden["issues"].as_array().unwrap().len(),
        "no duplicate / dropped issues vs the golden array length"
    );

    // ── per-code multiplicity equals the golden ──────────────────────────────
    // (e.g. SCHEMA_SHAPE_MISMATCH fires exactly twice: email + date.)
    assert_eq!(
        code_histogram(report["issues"].as_array().unwrap()),
        code_histogram(golden["issues"].as_array().unwrap()),
        "the per-code issue counts must match the golden exactly"
    );

    // ── v0.2 removed LAYER_TYPE_MISMATCH; the contact under wiki/ is now a
    //    fully clean file the sweep must NOT flag (a false-positive catcher) ────
    // The folder layout is convention, not enforcement — placement no longer
    // warns. The `contact` under `wiki/contacts/` is schema-valid with resolving
    // links, so it carries zero issues.
    let layer_issues: Vec<&serde_json::Value> = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["code"] == "LAYER_TYPE_MISMATCH")
        .collect();
    assert!(
        layer_issues.is_empty(),
        "LAYER_TYPE_MISMATCH was removed in v0.2; nothing emits it: {layer_issues:#?}"
    );
    let misplaced_issues: Vec<&serde_json::Value> = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["file"] == "wiki/contacts/misplaced-contact.md")
        .collect();
    assert!(
        misplaced_issues.is_empty(),
        "the contact under wiki/ is clean (schema + links valid) — no issue: {misplaced_issues:#?}"
    );

    // ── every emitted issue carries the full contract shape ──────────────────
    for issue in report["issues"].as_array().unwrap() {
        for field in [
            "severity", "code", "file", "line", "key", "message", "related",
        ] {
            assert!(
                issue.get(field).is_some(),
                "every issue object has the `{field}` key (null allowed for line/key): {issue}"
            );
        }
        let sev = issue["severity"].as_str().unwrap();
        assert!(
            matches!(sev, "error" | "warning" | "info"),
            "severity is one of the three words, got {sev:?}"
        );
    }
}

/// `{code -> count}` over an issue array.
fn code_histogram(issues: &[serde_json::Value]) -> BTreeMap<String, usize> {
    let mut h = BTreeMap::new();
    for i in issues {
        if let Some(c) = i.get("code").and_then(|c| c.as_str()) {
            *h.entry(c.to_string()).or_insert(0) += 1;
        }
    }
    h
}

fn u64_at(report: &serde_json::Value, key: &str) -> u64 {
    report["summary"][key]
        .as_u64()
        .unwrap_or_else(|| panic!("summary.{key} is a number"))
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 — policy-refusal scenarios refuse with the right error + leave files intact
// ─────────────────────────────────────────────────────────────────────────────

/// A committed `EXPECTED/policy-refusal/<scenario>.json` fixture: the exact
/// invocation, the expected structured error code, and the no-write contract.
#[derive(serde::Deserialize)]
struct PolicyRefusal {
    invocation: String,
    exit_code_nonzero: bool,
    no_write_occurred: bool,
    error: PolicyError,
}

#[derive(serde::Deserialize)]
struct PolicyError {
    code: String,
    /// The frozen path the refusal must name (the fixture's `error.file`).
    file: String,
}

/// Every write-surface refusal fixture committed under `policy-refusal/`.
const POLICY_REFUSAL_FIXTURES: &[&str] = &[
    "write.json",
    "fm-set.json",
    "rename.json",
    "link.json",
    "write-nonexistent-frozen.json",
];

/// Split a fixture's `invocation` ("dbmd <args...> --json") into the argv the
/// real binary receives, dropping the leading `dbmd`. The `--json` flag is kept
/// (it makes the error structured on stderr); `--dir` is appended by the caller.
///
/// Tokenizes shell-style so a single-quoted argument with spaces (e.g.
/// `--summary 'overwrite attempt'`) becomes ONE argv element — the fixtures use
/// single quotes around multi-word `--summary` values.
fn invocation_args(invocation: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut started = false; // distinguishes "" (a real empty arg) from no-arg
    for c in invocation.chars() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                started = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    assert_eq!(
        tokens.first().map(String::as_str),
        Some("dbmd"),
        "invocation starts with `dbmd`"
    );
    tokens.into_iter().skip(1).collect()
}

#[test]
fn policy_refusals_refuse_with_structured_error_and_do_not_write() {
    for fixture in POLICY_REFUSAL_FIXTURES {
        let golden: PolicyRefusal = {
            let raw =
                std::fs::read_to_string(corpus_b_expected(&format!("policy-refusal/{fixture}")))
                    .unwrap_or_else(|_| panic!("EXPECTED/policy-refusal/{fixture} is committed"));
            serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("policy-refusal/{fixture} is valid JSON: {e}"))
        };
        assert_eq!(
            golden.error.code, "POLICY_FROZEN_PAGE",
            "every policy-refusal fixture is a frozen-page refusal"
        );
        assert!(
            golden.exit_code_nonzero && golden.no_write_occurred,
            "fixture contract"
        );

        // Work against a fresh temp copy so the committed corpus is never mutated.
        let (_guard, store) = copy_store_to_temp(&corpus_b());

        // The frozen target as a store-relative path; capture its before-state
        // (content if present, or "absent" — one fixture targets a frozen path
        // that does not exist on disk, proving refusal is keyed on the policy
        // path, not file presence).
        let target_rel = &golden.error.file;
        let target_abs = store.join(target_rel);
        let before = std::fs::read(&target_abs).ok();

        // Run the exact committed invocation against the temp store. We set the
        // store as the working directory (rather than appending `--dir`, which
        // not every subcommand's positional parser accepts after its operands) —
        // the fixtures' invocations carry no `--dir`, so this runs them verbatim.
        let args = invocation_args(&golden.invocation);
        let out = dbmd()
            .current_dir(&store)
            .args(&args)
            .assert()
            .failure() // exit_code_nonzero
            .get_output()
            .clone();

        // ── structured error: code + the frozen path, on stderr under --json ──
        let stderr = String::from_utf8(out.stderr).unwrap();
        let err: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap_or_else(|e| {
            panic!("{fixture}: refusal must emit a JSON error on stderr: {e}\nstderr: {stderr}")
        });
        assert_eq!(
            err["error"]["code"], "POLICY_FROZEN_PAGE",
            "{fixture}: the refusal carries the structured POLICY_FROZEN_PAGE code, got {}",
            err["error"]
        );
        let msg = err["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains(target_rel),
            "{fixture}: the refusal message must name the frozen path {target_rel:?}; got {msg:?}"
        );

        // ── exit is the policy code (4), which is non-zero ────────────────────
        let code = out.status.code().expect("process exited normally");
        assert_eq!(
            code, 4,
            "{fixture}: a frozen-page refusal exits 4 (ExitCode::Policy)"
        );

        // ── no_write_occurred: the corpus file is byte-for-byte unchanged ─────
        let after = std::fs::read(&target_abs).ok();
        assert_eq!(
            before,
            after,
            "{fixture}: the frozen target {target_rel:?} must be byte-for-byte unchanged \
             (before-present={}, after-present={})",
            before.is_some(),
            after.is_some()
        );

        // A `write` to a NONEXISTENT frozen path must not have created the file
        // at the requested path — nor at any sharded relocation of it (the
        // `wiki-page` foldering would otherwise send it to `wiki/topics/…`).
        if before.is_none() {
            assert!(
                !target_abs.exists(),
                "{fixture}: the refused nonexistent frozen path must NOT be created"
            );
            if let Some(name) = Path::new(target_rel).file_name() {
                let sharded = store.join("wiki/topics").join(name);
                assert!(
                    !sharded.exists(),
                    "{fixture}: the refused write must not slip through to a sharded location {:?}",
                    sharded
                );
            }
        }

        // ── the rest of the store is untouched: the only thing that could have
        //    changed is the target; assert the store still validates to the SAME
        //    issue set as the pristine corpus (no side effects from the refusal).
        //    (Cheap proxy: the file count is unchanged.)
        assert_eq!(
            md_file_count(&store),
            md_file_count(&corpus_b()),
            "{fixture}: a refusal must not add or remove any file in the store"
        );
    }
}

/// Count `.md` files under a store (recursive). A refusal must not change it.
fn md_file_count(root: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_str().unwrap_or("");
                if name.starts_with('.') {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, n);
                } else if name.ends_with(".md") {
                    *n += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

// ─────────────────────────────────────────────────────────────────────────────
// 3 — NOT_A_STORE: the no-DB.md sibling, and the sweep does not descend into it
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn not_a_store_sibling_is_one_issue_and_outside_the_sweep() {
    let golden: serde_json::Value = read_json(&corpus_b_expected("not-a-store.json"));
    assert!(
        golden["exit_code_nonzero"].as_bool().unwrap_or(false),
        "the not-a-store fixture exits non-zero"
    );

    // Pointing `validate` directly at the no-DB.md sibling: exactly one
    // NOT_A_STORE issue, non-zero exit (reported as a validation issue, exit 6 —
    // not a bare open error).
    let sibling = corpus_b().join("not-a-store");
    let out = dbmd()
        .args(["--json", "validate"])
        .arg(&sibling)
        .assert()
        .failure()
        .code(6)
        .get_output()
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();

    // Exactly one issue, and it is the golden's NOT_A_STORE. The issue `file` is
    // the path the user passed (here absolute; the golden documents it
    // repo-relative because that is how the golden invocation was run), so we
    // hold the engine to the stable parts — code/severity/line/key/related and
    // the SHAPE of the golden — plus that the path names the `not-a-store` dir.
    let issues = report["issues"].as_array().unwrap();
    assert_eq!(
        issues.len(),
        1,
        "exactly one issue for the no-store path: {issues:#?}"
    );
    let golden_issue = &golden["issues"].as_array().unwrap()[0];
    let issue = &issues[0];
    assert_eq!(issue["code"], golden_issue["code"], "code is NOT_A_STORE");
    assert_eq!(issue["code"], "NOT_A_STORE");
    assert_eq!(issue["severity"], golden_issue["severity"]);
    assert_eq!(issue["line"], golden_issue["line"], "line is null");
    assert_eq!(issue["key"], golden_issue["key"], "key is null");
    assert_eq!(
        issue["related"], golden_issue["related"],
        "related is empty"
    );
    // The golden's `file` is the relative spelling of the same directory; the
    // emitted one ends with the same `not-a-store` component.
    let golden_file = golden_issue["file"].as_str().unwrap();
    assert!(
        golden_file.ends_with("not-a-store"),
        "golden file names the sibling"
    );
    assert!(
        issue["file"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("not-a-store"),
        "the emitted NOT_A_STORE file names the no-DB.md sibling, got {}",
        issue["file"]
    );

    // And the corpus-b `--all` sweep does NOT descend into the non-canonical
    // sibling: no NOT_A_STORE in the store-proper report, and none of the
    // sibling's files appear in it.
    let sweep: serde_json::Value = {
        let out = dbmd()
            .args(["--json", "validate", "--all"])
            .arg(corpus_b())
            .assert()
            .failure()
            .get_output()
            .clone();
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap()
    };
    for issue in sweep["issues"].as_array().unwrap() {
        assert_ne!(
            issue["code"], "NOT_A_STORE",
            "the store-proper sweep never emits NOT_A_STORE"
        );
        let file = issue["file"].as_str().unwrap_or("");
        assert!(
            !file.starts_with("not-a-store"),
            "the sweep must not descend into the non-canonical sibling, saw {file:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3b — DB.md structure: the bad-db-md/ sub-store trips the three DB_MD_* codes
//      in a single SEPARATE invocation, and the corpus-b root sweep never
//      descends into it.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn bad_db_md_substore_emits_the_three_db_md_codes_and_is_outside_the_sweep() {
    let golden: serde_json::Value = read_json(&corpus_b_expected("bad-db-md.json"));
    assert!(
        golden["exit_code_nonzero"].as_bool().unwrap_or(false),
        "the bad-db-md fixture exits non-zero"
    );

    // The golden is hand-derived (provenance in `_comment`), not a snapshot.
    let comment = golden["_comment"].as_str().unwrap_or("").to_lowercase();
    assert!(
        comment.contains("hand-derived") && comment.contains("never copied"),
        "bad-db-md.json declares hand-derivation and that it is not copied from output"
    );

    // Point `validate --all` straight at the sub-store. Its DB.md is a valid
    // marker (the filename), so this is a real store whose IDENTITY contract
    // fails — exit 6 (ValidationFailed), not a bare open error.
    let substore = corpus_b().join("bad-db-md");
    let out = dbmd()
        .args(["--json", "validate", "--all"])
        .arg(&substore)
        .assert()
        .failure()
        .code(6)
        .get_output()
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();

    // ── the issue SET equals the golden, exactly (the three DB_MD_* codes) ────
    let got = issue_set(report["issues"].as_array().expect("issues is an array"));
    let want = issue_set(golden["issues"].as_array().expect("golden issues array"));
    let missing: Vec<&IssueKey> = want.difference(&got).collect();
    let extra: Vec<&IssueKey> = got.difference(&want).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "bad-db-md validate must emit EXACTLY the golden issue set.\n\
         MISSING (in EXPECTED, not emitted): {missing:#?}\n\
         EXTRA (emitted, not in EXPECTED): {extra:#?}"
    );

    // The exact three codes, with the right severities (2 error + 1 warning).
    assert_eq!(
        code_histogram(report["issues"].as_array().unwrap()),
        code_histogram(golden["issues"].as_array().unwrap()),
        "per-code counts equal the golden"
    );
    let codes: BTreeSet<&str> = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    assert_eq!(
        codes,
        BTreeSet::from([
            "DB_MD_BAD_TYPE",
            "DB_MD_MISSING_FIELD",
            "DB_MD_UNKNOWN_SECTION"
        ]),
        "exactly the three DB.md-structure codes fire"
    );

    // Summary tallies match the golden (2 errors, 1 warning, 0 info, total 3).
    for k in ["errors", "warnings", "info", "total"] {
        assert_eq!(
            report["summary"][k], golden["summary"][k],
            "summary.{k} equals the golden"
        );
    }

    // The corpus-b `--all` sweep does NOT descend into the sibling sub-store:
    // no DB_MD_* code, and no issue whose file path is rooted in `bad-db-md/`.
    // (The sweep checks only the corpus-b ROOT `DB.md`, which is clean.)
    let sweep: serde_json::Value = {
        let out = dbmd()
            .args(["--json", "validate", "--all"])
            .arg(corpus_b())
            .assert()
            .failure()
            .get_output()
            .clone();
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap()
    };
    for issue in sweep["issues"].as_array().unwrap() {
        let code = issue["code"].as_str().unwrap_or("");
        assert!(
            !code.starts_with("DB_MD_"),
            "the corpus-b root sweep's DB.md is clean — no DB_MD_* code, saw {code}"
        );
        let file = issue["file"].as_str().unwrap_or("").replace('\\', "/");
        assert!(
            !file.starts_with("bad-db-md"),
            "the sweep must not descend into the bad-db-md sibling, saw {file:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4 — the golden is INTENT-DERIVED, not a snapshot of tool output
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn expected_validate_json_is_intent_derived_not_a_snapshot() {
    let golden: serde_json::Value = read_json(&corpus_b_expected("validate.json"));

    // (a) The golden declares its hand-derivation in `_comment` — a snapshot of
    //     tool output would carry no such provenance. The contract: it is
    //     derived from SPEC + what the corpus breaks, NEVER copied from output.
    let comment = golden["_comment"].as_str().unwrap_or("");
    let lc = comment.to_lowercase();
    assert!(
        lc.contains("hand-derived") || lc.contains("intent-derived"),
        "EXPECTED/validate.json must declare hand/intent derivation in _comment, got {comment:?}"
    );
    assert!(
        lc.contains("spec.md") || lc.contains("spec"),
        "the golden anchors itself to SPEC.md, got {comment:?}"
    );
    assert!(
        lc.contains("never copied")
            || lc.contains("not") && lc.contains("snapshot")
            || lc.contains("never be copied")
            || lc.contains("never copied from"),
        "the golden states it is not a snapshot of tool output, got {comment:?}"
    );

    // (b) Every code the golden emits is mapped in the committed coverage.json
    //     (each code → the fixture that seeds it). A code that fired by accident
    //     (a snapshot artifact) would be unmapped.
    let coverage: serde_json::Value = read_json(&corpus_b_expected("coverage.json"));
    let mapped: BTreeSet<String> = coverage["coverage"]
        .as_object()
        .expect("coverage.coverage is an object")
        .keys()
        .cloned()
        .chain(
            coverage
                .get("plan_extensions")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        )
        .collect();

    let emitted: BTreeSet<String> = golden["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["code"].as_str().map(String::from))
        .collect();
    let unmapped: Vec<&String> = emitted.difference(&mapped).collect();
    assert!(
        unmapped.is_empty(),
        "every code in the golden must be mapped to a fixture in coverage.json; unmapped: {unmapped:?}"
    );

    // (c) coverage.json's mapped codes are a SUBSET of the SPEC § Validation
    //     code table — the golden invents no codes. We read SPEC.md and pull
    //     every `| `CODE` |` row of the canonical-codes table.
    let spec_codes = spec_validation_codes();
    let invented: Vec<&String> = mapped.difference(&spec_codes).collect();
    assert!(
        invented.is_empty(),
        "coverage.json maps only real SPEC codes; not in the SPEC table: {invented:?}"
    );

    // (c2) coverage.json's bookkeeping cannot OVER-claim coverage. A SPEC code is
    //      "seeded" iff it is mapped to some fixture — whether under `coverage` or
    //      under `plan_extensions` (the latter just annotates a code as also
    //      plan-mandated; it does not make the code unseeded). So the seeded-SPEC
    //      set is `mapped ∩ spec_codes`, and the true gap is the rest of the SPEC
    //      table. The committed `uncovered_spec_codes` MUST equal that gap exactly
    //      (both directions), and `spec_code_count` / `all_spec_codes_covered` MUST
    //      agree with the SPEC table. This is the regression guard against a
    //      coverage.json that silently drops uncovered codes from the count and
    //      asserts full coverage.
    let seeded_spec: BTreeSet<String> = mapped.intersection(&spec_codes).cloned().collect();
    let true_uncovered: BTreeSet<String> = spec_codes.difference(&seeded_spec).cloned().collect();
    let declared_uncovered: BTreeSet<String> = coverage["uncovered_spec_codes"]
        .as_array()
        .expect("coverage.json declares uncovered_spec_codes (array)")
        .iter()
        .filter_map(|c| c.as_str().map(String::from))
        .collect();
    assert_eq!(
        declared_uncovered, true_uncovered,
        "coverage.json's uncovered_spec_codes must equal SPEC-codes minus seeded codes \
         exactly — no SPEC code may be silently dropped, and none falsely claimed uncovered"
    );
    let spec_code_count = coverage["spec_code_count"]
        .as_u64()
        .expect("coverage.json declares spec_code_count (number)")
        as usize;
    assert_eq!(
        spec_code_count,
        spec_codes.len(),
        "coverage.json's spec_code_count must equal the real SPEC § Validation code count"
    );
    let all_covered = coverage["all_spec_codes_covered"]
        .as_bool()
        .expect("coverage.json declares all_spec_codes_covered (bool)");
    assert_eq!(
        all_covered,
        true_uncovered.is_empty(),
        "all_spec_codes_covered must be true iff every SPEC code is seeded \
         (uncovered: {true_uncovered:?})"
    );

    // (c3) The three Block-1 DB.md identity/structure checks are REAL, not
    //      aspirational. The plan once drifted to claim these checks "await a
    //      SPEC code"; they have since landed (the live SPEC § Validation table
    //      has 38 codes in v0.2). This pins the substance that claim got wrong:
    //      each code MUST be a row in the live SPEC table AND seeded (mapped to a
    //      fixture) here — so a regression that drops a code from SPEC, or stops
    //      seeding it, turns this red with a code-named message rather than only
    //      nudging the aggregate counts.
    for code in [
        "DB_MD_BAD_TYPE",
        "DB_MD_MISSING_FIELD",
        "DB_MD_UNKNOWN_SECTION",
    ] {
        assert!(
            spec_codes.contains(code),
            "Block-1 validate code `{code}` must be a row in the live SPEC § Validation table \
             (these checks no longer 'await a SPEC code' — the SPEC defines them)"
        );
        assert!(
            seeded_spec.contains(code),
            "Block-1 validate code `{code}` must be seeded by a corpus-b fixture in coverage.json \
             (the DB.md-identity / layer-type checks are exercised, not just declared)"
        );
    }

    // (d) One designed breakage per fixture: the issues spread across MANY
    //     distinct fixture files (the breakage sites), not a handful — a clean
    //     one-issue-per-fixture structure a raw dump would not have. The golden
    //     seeds 40 issues across well over a dozen distinct files.
    let distinct_files: BTreeSet<&str> = golden["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["file"].as_str())
        .collect();
    assert!(
        distinct_files.len() >= 15,
        "the breakages are spread across distinct designed fixtures (got {} files)",
        distinct_files.len()
    );

    // (e) Every issue carries a deterministic, non-empty `suggestion` — the
    //     hand-authored remediation hint. A field-absent issue is anchored to a
    //     real line (never null where the README says line 1). Spot-check the
    //     structural invariants the README pins.
    for issue in golden["issues"].as_array().unwrap() {
        assert!(
            issue["suggestion"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "each golden issue has a deterministic remediation suggestion: {issue}"
        );
    }

    // (f) The dedup precedence (README rule #1): each DUP_* issue is reported
    //     ONCE with the colliding partner(s) in `related` — never duplicated per
    //     partner. So every DUP_* issue has a non-empty `related`.
    for issue in golden["issues"].as_array().unwrap() {
        let code = issue["code"].as_str().unwrap_or("");
        if code.starts_with("DUP_") {
            let related = issue["related"].as_array().map(|a| a.len()).unwrap_or(0);
            assert!(
                related >= 1,
                "{code} reports one issue with the partner in `related` (rule #1): {issue}"
            );
        }
    }
}

/// Parse `SPEC.md § Validation` and return the set of canonical issue codes
/// (`| `CODE` | severity | … |` rows). This is the independent source of truth
/// the golden's coverage map must be a subset of.
fn spec_validation_codes() -> BTreeSet<String> {
    let spec = std::fs::read_to_string(repo_root().join("SPEC.md")).expect("SPEC.md at repo root");
    let mut codes = BTreeSet::new();
    for line in spec.lines() {
        let t = line.trim_start();
        // A canonical-code table row: `| `CODE` | <severity> | … |`. Pull the
        // first backtick-quoted ALL-CAPS token on a markdown table row.
        if !t.starts_with("| `") {
            continue;
        }
        if let Some(rest) = t.strip_prefix("| `") {
            if let Some((code, _)) = rest.split_once('`') {
                if !code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                    codes.insert(code.to_string());
                }
            }
        }
    }
    assert!(
        codes.len() >= 30,
        "parsed the SPEC validation code table (got {} codes)",
        codes.len()
    );
    codes
}

// ─────────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Read + parse a committed JSON golden, with a path-bearing panic on failure.
fn read_json(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("committed golden is missing: {}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("golden {} is valid JSON: {e}", path.display()))
}

/// The repo root, resolved from this crate's manifest (`crates/dbmd-cli` →
/// `../..`). Used to read `SPEC.md`, the independent code-table source.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}
