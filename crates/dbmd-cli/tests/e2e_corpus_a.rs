//! End-to-end happy-path integration test for the canonical store
//! (`tests/corpora/corpus-a-canonical`), driving the **real `dbmd` binary** as a
//! subprocess (`assert_cmd`) — not in-process library calls. This is the single
//! test that exercises a full agent session shape over the golden corpus:
//!
//!   1. **`validate --all`** returns ZERO issues and its JSON `issues` array
//!      equals the committed `EXPECTED/validate.json` (`[]`).
//!   2. Every case in **`EXPECTED/search.json`** returns exactly the golden file
//!      set — run through the binary in both text (`file:line:text`) and `--json`
//!      modes, with the same `--type` / `--in` / `--updated-after` filters the
//!      golden records.
//!   3. A **parse → `fm set summary` → re-parse round-trip** preserves the body
//!      byte-for-byte and applies the new summary (read back via `fm get`).
//!   4. **`index rebuild --dry-run`** is parsed into per-artifact text, AUDITED
//!      against the SPEC §"`index.md` and `index.jsonl`" format rules (cap=500,
//!      `## More` overflow footer, `index.jsonl` completeness, summary-verbatim),
//!      and asserted byte-identical to the committed `EXPECTED/index/<path>`
//!      goldens (whose audit produced them). The artifact *set + order* is locked
//!      to `EXPECTED/index/ARTIFACTS.txt`.
//!   5. A **synthetic over-cap** folder (501 records, built in a temp store)
//!      exercises the cap-at-500 + `## More` + uncapped-jsonl branch that
//!      corpus-a's largest folder (490 < 500) cannot reach — so the cap rule is
//!      audited on both sides of the threshold.
//!
//! The goldens are committed; this test is their executable contract. Run it
//! after any change that could move index/validate/search output:
//! `cargo test -p dbmd-cli --test e2e_corpus_a`.
//!
//! The remaining two corpus-a goldens — **`EXPECTED/graph.json`** (`graph
//! backlinks` + `graph neighborhood` for representative seeds) and
//! **`EXPECTED/log-tail.json`** (`log tail 20`, normalized) — have their own
//! executable contract in the sibling file `e2e_corpus_a_graph_log.rs`.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{
    copy_store_to_temp, corpus_a, corpus_a_expected, dbmd, split_frontmatter_body, write_db_md,
    write_file,
};

/// The SPEC cap on a type-folder `index.md` browse view (SPEC § "Cap: 500
/// entries per type-folder `index.md`"). The `index.jsonl` twin is uncapped.
const INDEX_CAP: usize = 500;

// ─────────────────────────────────────────────────────────────────────────────
// 1 — validate --all is clean and matches EXPECTED/validate.json
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_all_is_clean_and_matches_expected_golden() {
    // The canonical store is the happy path: a full SWEEP must find nothing.
    let out = dbmd()
        .args(["--json", "validate", "--all"])
        .arg(corpus_a())
        .assert()
        .success(); // exit 0 ⇒ zero errors
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("validate emits JSON");

    // Scope + tallies: a full sweep with an empty issue ledger.
    assert_eq!(report["scope"], "all", "`--all` is the full SWEEP scope");
    assert_eq!(report["summary"]["errors"], 0);
    assert_eq!(report["summary"]["warnings"], 0);
    assert_eq!(report["summary"]["info"], 0);
    assert_eq!(report["summary"]["total"], 0);

    // The golden contract: EXPECTED/validate.json is the exact `issues` array the
    // canonical store must produce — an empty list.
    let expected_issues: serde_json::Value = {
        let raw = std::fs::read_to_string(corpus_a_expected("validate.json"))
            .expect("EXPECTED/validate.json is committed");
        serde_json::from_str(&raw).expect("EXPECTED/validate.json is valid JSON")
    };
    assert_eq!(
        expected_issues,
        serde_json::json!([]),
        "the committed golden pins zero issues for the canonical store"
    );
    assert_eq!(
        report["issues"], expected_issues,
        "validate --all issues must equal EXPECTED/validate.json: got {}",
        report["issues"]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 — every EXPECTED/search.json case returns the golden file set
// ─────────────────────────────────────────────────────────────────────────────

/// One case from the search golden master.
#[derive(serde::Deserialize)]
struct SearchCase {
    query: String,
    #[serde(default)]
    args: Vec<String>,
    matches: Vec<String>,
}

/// The whole `EXPECTED/search.json` document (we only need `queries`).
#[derive(serde::Deserialize)]
struct SearchGolden {
    queries: Vec<SearchCase>,
}

/// Load the committed search golden master.
fn search_golden() -> Vec<SearchCase> {
    let raw = std::fs::read_to_string(corpus_a_expected("search.json"))
        .expect("EXPECTED/search.json is committed");
    let doc: SearchGolden = serde_json::from_str(&raw).expect("EXPECTED/search.json is valid JSON");
    assert!(
        !doc.queries.is_empty(),
        "the golden has at least one query case"
    );
    doc.queries
        .into_iter()
        .filter(|c| !c.query.is_empty())
        .collect()
}

/// Run `dbmd search <query> <case-args> --json --dir corpus-a` through the real
/// binary and return the deduped set of store-relative files that matched. The
/// golden asserts *files*, so we collapse the per-line `rg` matches to paths.
fn search_files_json(case: &SearchCase) -> BTreeSet<String> {
    let out = dbmd()
        .arg("--json")
        .arg("search")
        .arg(&case.query)
        .args(&case.args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let matches: serde_json::Value = serde_json::from_str(&stdout).expect("search --json is JSON");
    matches
        .as_array()
        .expect("search --json is an array")
        .iter()
        .map(|m| {
            m["file"]
                .as_str()
                .expect("each match has a file")
                .to_string()
        })
        .collect()
}

/// Run the same case in **text** mode (`file:line:text`) and return the file set,
/// taken from the substring before the first `:`. Proves the default rg-shaped
/// output agrees with `--json` on which files matched.
fn search_files_text(case: &SearchCase) -> BTreeSet<String> {
    let out = dbmd()
        .arg("search")
        .arg(&case.query)
        .args(&case.args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.split_once(':')
                .expect("rg-shaped output is file:line:text")
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn search_golden_cases_return_the_expected_files() {
    for case in search_golden() {
        let expected: BTreeSet<String> = case.matches.iter().cloned().collect();

        let json_files = search_files_json(&case);
        assert_eq!(
            json_files, expected,
            "search --json for {:?} {:?} must return the golden file set",
            case.query, case.args
        );

        // The human/rg text output must agree on the file set.
        let text_files = search_files_text(&case);
        assert_eq!(
            text_files, expected,
            "search (text mode) for {:?} {:?} must match the golden file set",
            case.query, case.args
        );
    }
}

#[test]
fn search_no_match_is_empty_success() {
    // A "not found" is data, not an error: exit 0, empty stdout.
    let out = dbmd()
        .arg("search")
        .arg("zzz-no-such-term-anywhere-zzz")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    assert!(
        out.get_output().stdout.is_empty(),
        "a no-match search prints nothing"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3 — parse → fm set summary → re-parse round-trip preserves body, applies change
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn fm_set_summary_round_trip_preserves_body_and_applies_change() {
    // Write into a temp copy so the committed corpus is never mutated.
    let (_guard, store) = copy_store_to_temp(&corpus_a());
    let rel = "records/contacts/marcus-okafor.md";
    let path = store.join(rel);

    // ── parse: capture the current summary (via the binary) + the body region ──
    let original_text = std::fs::read_to_string(&path).unwrap();
    let (_orig_fm, original_body) =
        split_frontmatter_body(&original_text).expect("the record opens with frontmatter");

    let old_summary = {
        let out = dbmd()
            .current_dir(&store)
            .args(["fm", "get", rel, "summary"])
            .assert()
            .success();
        String::from_utf8(out.get_output().stdout.clone())
            .unwrap()
            .trim()
            .to_string()
    };
    let new_summary = "Hand-curated: ops analyst who joined the Northstar renewal thread";
    assert_ne!(old_summary, new_summary, "the change must be observable");

    // ── fm set summary: apply the one-field change atomically ──────────────────
    dbmd()
        .current_dir(&store)
        .args(["fm", "set", rel, &format!("summary={new_summary}")])
        .assert()
        .success();

    // ── re-parse: body byte-identical, summary is the new value ────────────────
    let updated_text = std::fs::read_to_string(&path).unwrap();
    let (_new_fm, updated_body) =
        split_frontmatter_body(&updated_text).expect("the record still has frontmatter");
    assert_eq!(
        updated_body, original_body,
        "the operator-edited body must round-trip byte-for-byte across `fm set`"
    );

    let reparsed_summary = {
        let out = dbmd()
            .current_dir(&store)
            .args(["fm", "get", rel, "summary"])
            .assert()
            .success();
        String::from_utf8(out.get_output().stdout.clone())
            .unwrap()
            .trim()
            .to_string()
    };
    assert_eq!(
        reparsed_summary, new_summary,
        "re-parsing after `fm set` returns the new summary"
    );

    // Write-through to the sidecar: the new summary is in the type-folder
    // index.jsonl (the round-trip is observable through the catalog too).
    let jsonl = std::fs::read_to_string(store.join("records/contacts/index.jsonl")).unwrap();
    let marcus_line = jsonl
        .lines()
        .find(|l| l.contains("marcus-okafor"))
        .expect("marcus stays indexed");
    let rec: serde_json::Value = serde_json::from_str(marcus_line).unwrap();
    assert_eq!(
        rec["summary"],
        serde_json::json!(new_summary),
        "the sidecar carries the new summary verbatim after the round-trip"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 4 — index rebuild --dry-run: audit against SPEC + assert the committed goldens
// ─────────────────────────────────────────────────────────────────────────────

/// One emitted artifact from a `--dry-run` preview: its store-relative path and
/// its full would-be file content.
struct Artifact {
    path: String,
    content: String,
}

/// Parse an `index rebuild --dry-run` stdout into its ordered list of artifacts.
/// The dry-run format is a sequence of `--- <path> ---\n` separators, each
/// followed by that file's complete content up to the next separator (or EOF).
fn parse_dry_run(stdout: &str) -> Vec<Artifact> {
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cur_body = String::new();
    for line in stdout.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = trimmed.strip_prefix("--- ") {
            if let Some(path) = rest.strip_suffix(" ---") {
                if let Some(prev) = cur_path.take() {
                    artifacts.push(Artifact {
                        path: prev,
                        content: std::mem::take(&mut cur_body),
                    });
                }
                cur_path = Some(path.to_string());
                continue;
            }
        }
        if cur_path.is_some() {
            cur_body.push_str(line);
        }
    }
    if let Some(prev) = cur_path.take() {
        artifacts.push(Artifact {
            path: prev,
            content: cur_body,
        });
    }
    artifacts
}

/// Count the `- [[...]]` entry lines in an `index.md` body.
fn entry_lines(content: &str) -> Vec<&str> {
    content.lines().filter(|l| l.starts_with("- [[")).collect()
}

/// Read one frontmatter scalar (`scope`, `folder`, …) from an `index.md`.
fn fm_field<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    let (fm, _) = split_frontmatter_body(content)?;
    let needle = format!("{key}:");
    fm.lines()
        .find_map(|l| l.trim().strip_prefix(&needle).map(str::trim))
}

/// Count the real `.md` content files in a store folder (recursive, across
/// date-shards, excluding `index.md`). The denominator for the cap + jsonl
/// completeness audits.
fn count_md_files(folder: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, n);
                } else if p.extension().and_then(|x| x.to_str()) == Some("md")
                    && p.file_name().and_then(|x| x.to_str()) != Some("index.md")
                {
                    *n += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(folder, &mut n);
    n
}

/// AUDIT one dry-run artifact set against the SPEC § "`index.md` and
/// `index.jsonl`" format rules, resolving file counts against `store_root`:
///
/// - **cap=500 + `## More`** — a type-folder `index.md` lists every file when the
///   folder is `<= 500`, and never carries a `## More` footer; over the cap it
///   lists exactly 500 and DOES carry the footer.
/// - **jsonl completeness** — each type-folder `index.jsonl` has exactly one
///   valid-JSON object per `.md` file (uncapped), each with the universal keys.
/// - **summary-verbatim** — every `index.md` entry quotes its file's `summary`
///   (the value in the matching `index.jsonl` record), with no recomputation.
///
/// Panics with a precise message on the first violation. Returns the number of
/// type-folders audited (so the caller can assert it audited something).
fn audit_index_artifacts(artifacts: &[Artifact], store_root: &Path) -> usize {
    use std::collections::BTreeMap;

    // Index artifacts by path for cross-referencing md ↔ jsonl.
    let by_path: BTreeMap<&str, &str> = artifacts
        .iter()
        .map(|a| (a.path.as_str(), a.content.as_str()))
        .collect();

    let mut type_folders_audited = 0;

    // ── jsonl completeness (+ valid JSON + universal keys) ───────────────────
    for a in artifacts.iter().filter(|a| a.path.ends_with("index.jsonl")) {
        let folder = a
            .path
            .strip_suffix("/index.jsonl")
            .expect("jsonl path ends with /index.jsonl");
        let lines: Vec<&str> = a.content.lines().filter(|l| !l.trim().is_empty()).collect();
        let n_files = count_md_files(&store_root.join(folder));
        assert_eq!(
            lines.len(),
            n_files,
            "jsonl completeness: {} has {} lines but the folder has {} .md files (the jsonl twin is uncapped + complete)",
            a.path,
            lines.len(),
            n_files
        );
        for l in &lines {
            let rec: serde_json::Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("{}: bad JSON line: {e}\n{l}", a.path));
            for key in [
                "path", "type", "summary", "tags", "links", "created", "updated",
            ] {
                assert!(
                    rec.get(key).is_some(),
                    "jsonl record in {} is missing universal key `{key}`: {l}",
                    a.path
                );
            }
        }
    }

    // ── per-index.md: cap + ## More + summary-verbatim ───────────────────────
    for a in artifacts.iter().filter(|a| a.path.ends_with("index.md")) {
        let scope = fm_field(&a.content, "scope").unwrap_or("");
        // Cap + summary-verbatim only apply to type-folder indexes (root/layer
        // are count rollups, not file listings).
        if scope != "type-folder" {
            continue;
        }
        type_folders_audited += 1;
        let folder = fm_field(&a.content, "folder").expect("type-folder index has a folder field");
        let n_files = count_md_files(&store_root.join(folder));
        let entries = entry_lines(&a.content);
        let has_more = a.content.contains("## More");

        if n_files <= INDEX_CAP {
            assert_eq!(
                entries.len(),
                n_files,
                "cap (under): {} lists {} entries but the folder has {} files — all must be listed",
                a.path,
                entries.len(),
                n_files
            );
            assert!(
                !has_more,
                "cap (under): {} has a `## More` footer but is under the {INDEX_CAP} cap ({n_files} files)",
                a.path
            );
        } else {
            assert_eq!(
                entries.len(),
                INDEX_CAP,
                "cap (over): {} lists {} entries; the browse view caps at {INDEX_CAP}",
                a.path,
                entries.len()
            );
            assert!(
                has_more,
                "cap (over): {} is over the cap ({n_files} > {INDEX_CAP}) but has no `## More` footer",
                a.path
            );
            assert!(
                a.content
                    .contains(&format!("This folder has {n_files} files")),
                "the `## More` footer must state the true file count ({n_files}) in {}",
                a.path
            );
            assert!(
                a.content.contains("dbmd index query"),
                "the `## More` footer must point at `dbmd index query` for the complete catalog in {}",
                a.path
            );
        }

        // summary-verbatim: each entry quotes the file's summary, sourced from
        // the matching jsonl record (no recomputation, no drift).
        let jsonl = by_path
            .get(format!("{folder}/index.jsonl").as_str())
            .expect("every type-folder index.md has a jsonl twin in the same dry-run");
        let summaries: std::collections::HashMap<String, String> = jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let rec: serde_json::Value = serde_json::from_str(l).unwrap();
                (
                    rec["path"].as_str().unwrap().to_string(),
                    rec["summary"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let mut checked = 0;
        for entry in &entries {
            // `- [[<target>]] — <text>` ; target has no `.md` suffix.
            let inner = entry
                .strip_prefix("- [[")
                .and_then(|s| s.split_once("]]"))
                .map(|(t, rest)| (t.to_string(), rest));
            let Some((target, rest)) = inner else {
                continue;
            };
            let summary = match summaries.get(&format!("{target}.md")) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            assert!(
                rest.contains(summary.as_str()),
                "summary-verbatim: entry for {target} in {} does not quote its summary {summary:?} (entry: {entry:?})",
                a.path
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "summary-verbatim: audited 0 entries for {} — the audit must check something",
            a.path
        );
    }

    type_folders_audited
}

#[test]
fn index_rebuild_dry_run_audits_clean_and_matches_committed_goldens() {
    // Run the dry-run preview of a full rebuild over the committed corpus.
    let out = dbmd()
        .current_dir(corpus_a())
        .args(["index", "rebuild", "--dry-run"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let artifacts = parse_dry_run(&stdout);
    assert!(
        !artifacts.is_empty(),
        "the dry-run previews at least one artifact"
    );

    // ── AUDIT against the SPEC format rules (cap / ## More / jsonl / summary) ──
    let audited = audit_index_artifacts(&artifacts, &corpus_a());
    assert!(
        audited >= 5,
        "audited {audited} type-folders — the corpus has several"
    );

    // ── Assert the emitted set + order equals EXPECTED/index/ARTIFACTS.txt ────
    let manifest = std::fs::read_to_string(corpus_a_expected("index/ARTIFACTS.txt"))
        .expect("EXPECTED/index/ARTIFACTS.txt is committed");
    let expected_order: Vec<&str> = manifest.lines().filter(|l| !l.is_empty()).collect();
    let got_order: Vec<&str> = artifacts.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(
        got_order, expected_order,
        "the dry-run must emit exactly the golden artifact set, in the golden order"
    );

    // ── Assert each artifact is byte-identical to its committed golden ────────
    for a in &artifacts {
        let golden_path = corpus_a_expected(&format!("index/{}", a.path));
        let golden = std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|_| panic!("missing committed golden: EXPECTED/index/{}", a.path));
        assert_eq!(
            a.content, golden,
            "dry-run output for {} must be byte-identical to its audited golden EXPECTED/index/{}",
            a.path, a.path
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5 — synthetic over-cap: the cap=500 + ## More + uncapped-jsonl SPEC branch
//     (corpus-a's largest folder is 490 < 500, so it cannot reach this path)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn index_rebuild_audits_the_over_cap_more_branch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path();
    write_db_md(store);

    // 501 note records — one over the cap. `updated` strictly descending so the
    // recency order (and thus which 500 survive into index.md) is deterministic.
    let total = INDEX_CAP + 1;
    for i in 0..total {
        let day = 1 + (i % 28);
        let month = 1 + (i / 28) % 12;
        let ts = format!("2026-{month:02}-{day:02}T00:00:00Z");
        write_file(
            store,
            &format!("records/notes/n{i:04}.md"),
            &format!(
                "---\ntype: note\ncreated: {ts}\nupdated: {ts}\nsummary: note number {i}\n---\n\n# Note {i}\n"
            ),
        );
    }

    // Real rebuild (write, not dry-run) so we can audit the artifacts on disk.
    dbmd()
        .current_dir(store)
        .args(["index", "rebuild", "--folder", "records/notes"])
        .assert()
        .success();

    let index_md = std::fs::read_to_string(store.join("records/notes/index.md")).unwrap();
    let index_jsonl = std::fs::read_to_string(store.join("records/notes/index.jsonl")).unwrap();

    // Reuse the same SPEC audit on the synthetic artifacts: this exercises the
    // over-cap branch (entries == 500, `## More` present, footer states 501).
    let artifacts = vec![
        Artifact {
            path: "records/notes/index.md".into(),
            content: index_md.clone(),
        },
        Artifact {
            path: "records/notes/index.jsonl".into(),
            content: index_jsonl.clone(),
        },
    ];
    let audited = audit_index_artifacts(&artifacts, store);
    assert_eq!(
        audited, 1,
        "exactly the one synthetic type-folder is audited"
    );

    // Pin the over-cap specifics the shared audit asserts, spelled out here so
    // the over-cap contract is legible at the test site too.
    assert_eq!(
        entry_lines(&index_md).len(),
        INDEX_CAP,
        "index.md caps the browse view at {INDEX_CAP} entries"
    );
    assert!(
        index_md.contains("## More"),
        "over-cap index.md carries the ## More footer"
    );
    assert!(
        index_md.contains(&format!("This folder has {total} files")),
        "the footer states the true (uncapped) file count"
    );
    assert_eq!(
        index_jsonl.lines().filter(|l| !l.trim().is_empty()).count(),
        total,
        "index.jsonl is the uncapped, complete twin ({total} records)"
    );
}
