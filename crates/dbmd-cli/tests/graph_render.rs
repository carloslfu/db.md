//! Integration tests for the Block 5 read/render subcommands —
//! `dbmd graph {backlinks,forwardlinks,neighborhood,orphans}`, `dbmd tree`,
//! `dbmd stats`, and `dbmd outline`.
//!
//! These drive the real `dbmd` binary (`CARGO_BIN_EXE_dbmd`) against the
//! committed `corpus-a-canonical` fixture and assert on intent — the paths,
//! counts, and structure the SPEC + the corpus content imply — never on a blob
//! copied back from the tool's own output. Each command is exercised in both
//! the default text mode and `--json`, plus its flags (`--limit`, `--in`,
//! `--type`, `--hops`). The handlers are thin wrappers, so a green run here is
//! evidence the wrapper wired `dbmd-core` up correctly: right store, right args,
//! right formatting.

use std::path::PathBuf;
use std::process::Command;

/// Absolute path to the `corpus-a-canonical` fixture, resolved from the crate
/// manifest dir so the tests are CWD-independent.
fn corpus_a() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpora/corpus-a-canonical")
        .canonicalize()
        .expect("corpus-a-canonical must exist")
}

/// Run `dbmd <args...>` and return `(exit_code, stdout, stderr)`. The binary is
/// the one Cargo built for this integration test.
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dbmd"))
        .args(args)
        .output()
        .expect("failed to spawn dbmd");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout utf8"),
        String::from_utf8(output.stderr).expect("stderr utf8"),
    )
}

/// Run a command expected to succeed (exit 0); panic with stderr otherwise, and
/// return stdout.
fn run_ok(args: &[&str]) -> String {
    let (code, out, err) = run(args);
    assert_eq!(code, 0, "`dbmd {args:?}` exited {code}; stderr:\n{err}");
    out
}

/// The non-empty lines of some stdout, trimmed of the trailing newline.
fn lines(s: &str) -> Vec<String> {
    s.lines().map(|l| l.to_string()).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// graph backlinks
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn backlinks_lists_incoming_content_links_sorted_excluding_indexes() {
    // sarah-chen is referenced by five CONTENT files in the corpus (the company
    // record, two meetings, her wiki bio, and the renewal project). The
    // `records/contacts/index.md` catalog also names her, but indexes are
    // catalog, not relationship edges, so backlinks must exclude it. Output is
    // the canonical bare wiki-link form (no `.md`), sorted ascending.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "records/companies/northstar",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
            "wiki/people/sarah-chen",
            "wiki/projects/northstar-renewal",
        ]
    );
}

#[test]
fn backlinks_json_is_a_sorted_string_array() {
    let dir = corpus_a();
    let out = run_ok(&[
        "--json",
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let arr: Vec<String> = serde_json::from_str(out.trim()).expect("backlinks json array");
    assert_eq!(
        arr,
        vec![
            "records/companies/northstar".to_string(),
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review".to_string(),
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call".to_string(),
            "wiki/people/sarah-chen".to_string(),
            "wiki/projects/northstar-renewal".to_string(),
        ]
    );
}

#[test]
fn backlinks_honors_limit() {
    // `--limit` is a presentation cap applied to the sorted result, so the first
    // N entries are deterministic.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--dir",
        dir.to_str().unwrap(),
        "--limit",
        "2",
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "records/companies/northstar",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
        ]
    );
}

#[test]
fn backlinks_of_a_target_nobody_links_is_empty_exit_zero() {
    // The Marcus-intro source email is an orphan in the corpus: nothing
    // wiki-links to it, so backlinks is empty — and an empty result is success
    // (exit 0, no stdout), not an error.
    let dir = corpus_a();
    let (code, out, _err) = run(&[
        "graph",
        "backlinks",
        "sources/emails/2026/05/2026-05-12-marcus-intro.md",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert!(out.trim().is_empty(), "expected no backlinks, got:\n{out}");
}

#[test]
fn backlinks_type_filter_scopes_to_the_linking_files_type() {
    // sarah-chen's five backlinks span four types/layers; `--type meeting`
    // keeps only the two meeting records that reference her. The meeting linkers
    // carry her wiki-link in `attendees:` + their body — NOT in the sidecar's
    // `links` field (it is empty for them) — so a green assertion here is also
    // evidence the filter reads the right type-folder sidecar AND confirms the
    // edge by parsing the file.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--type",
        "meeting",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
        ]
    );
}

#[test]
fn backlinks_in_layer_filter_scopes_to_the_linking_files_layer() {
    // `--in wiki` keeps only the wiki-layer linkers (her bio + the renewal
    // project); the company + two meetings (records/) drop out.
    let dir = corpus_a();
    let d = dir.to_str().unwrap();
    let wiki = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--in",
        "wiki",
        "--dir",
        d,
    ]);
    assert_eq!(
        lines(&wiki),
        vec!["wiki/people/sarah-chen", "wiki/projects/northstar-renewal"]
    );

    // `--in records` keeps only the records-layer linkers.
    let records = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--in",
        "records",
        "--dir",
        d,
    ]);
    assert_eq!(
        lines(&records),
        vec![
            "records/companies/northstar",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
        ]
    );
}

#[test]
fn backlinks_type_and_in_compose() {
    // `--type meeting --in records` together still yield the two meetings;
    // `--type meeting --in wiki` yields nothing (no meeting lives under wiki/).
    let dir = corpus_a();
    let d = dir.to_str().unwrap();
    let both = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--type",
        "meeting",
        "--in",
        "records",
        "--dir",
        d,
    ]);
    assert_eq!(
        lines(&both),
        vec![
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
        ]
    );

    let mismatch = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--type",
        "meeting",
        "--in",
        "wiki",
        "--dir",
        d,
    ]);
    assert!(
        mismatch.trim().is_empty(),
        "no meeting linker lives under wiki/, got:\n{mismatch}"
    );
}

#[test]
fn forwardlinks_type_filter_narrows_targets_to_that_type() {
    // The renewal wiki page links out to eight targets across types; `--type
    // contact` keeps only the two contact targets.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "forwardlinks",
        "wiki/projects/northstar-renewal.md",
        "--type",
        "contact",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "records/contacts/elena-rodriguez",
            "records/contacts/sarah-chen",
        ]
    );
}

#[test]
fn forwardlinks_in_layer_filter_narrows_targets_to_that_layer() {
    // `--in sources` keeps only the source-layer targets of the renewal page
    // (the MSA doc + the Elena renewal email).
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "forwardlinks",
        "wiki/projects/northstar-renewal.md",
        "--in",
        "sources",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "sources/docs/2026-03-15-northstar-msa",
            "sources/emails/2026/05/2026-05-22-elena-renewal",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// graph forwardlinks
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn forwardlinks_returns_frontmatter_and_body_targets_sorted_deduped() {
    // The renewal wiki page links out — from BOTH its `derived_from` frontmatter
    // sequence and its body prose — to the company, two contacts, two meetings,
    // a source email, a source doc, and the synthesis plan. forwardlinks must
    // follow frontmatter edges too, dedup, drop self-links, and sort ascending.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "forwardlinks",
        "wiki/projects/northstar-renewal.md",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&out),
        vec![
            "records/companies/northstar",
            "records/contacts/elena-rodriguez",
            "records/contacts/sarah-chen",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
            "sources/docs/2026-03-15-northstar-msa",
            "sources/emails/2026/05/2026-05-22-elena-renewal",
            "wiki/synthesis/2026-renewal-plan",
        ]
    );
}

#[test]
fn forwardlinks_and_backlinks_round_trip_on_the_same_key() {
    // If the renewal page forwardlinks to sarah-chen, then sarah-chen backlinks
    // to the renewal page — both expressed in the identical bare key.
    let dir = corpus_a();
    let d = dir.to_str().unwrap();
    let fwd = run_ok(&[
        "graph",
        "forwardlinks",
        "wiki/projects/northstar-renewal.md",
        "--dir",
        d,
    ]);
    assert!(lines(&fwd).contains(&"records/contacts/sarah-chen".to_string()));

    let back = run_ok(&[
        "graph",
        "backlinks",
        "records/contacts/sarah-chen.md",
        "--dir",
        d,
    ]);
    assert!(back.lines().any(|l| l == "wiki/projects/northstar-renewal"));
}

// ─────────────────────────────────────────────────────────────────────────────
// graph neighborhood
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn neighborhood_one_hop_hydrates_summaries_in_json() {
    // One hop from sarah-chen reaches exactly her direct neighbors (both
    // directions): the company + renewal-call she links to, the renewal project,
    // and the two records that link to her. Each node carries the reached file's
    // real `summary` and `type`, at hop 1, with a `via` back toward the seed.
    let dir = corpus_a();
    let out = run_ok(&[
        "--json",
        "graph",
        "neighborhood",
        "records/contacts/sarah-chen.md",
        "--hops",
        "1",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("neighborhood json");
    assert_eq!(v["seed"], "records/contacts/sarah-chen");

    let nodes = v["nodes"].as_array().expect("nodes array");
    // The set of reached paths (order is BFS, so compare as a set).
    let mut paths: Vec<&str> = nodes.iter().map(|n| n["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "records/companies/northstar",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
            "wiki/people/sarah-chen",
            "wiki/projects/northstar-renewal",
        ]
    );

    // Every node is at hop 1, carries a non-empty summary + a type, and a `via`
    // edge that points back to the seed.
    for n in nodes {
        assert_eq!(n["hops"], 1, "node {n} should be one hop out");
        assert!(
            !n["summary"].as_str().unwrap().is_empty(),
            "node {n} must carry the reached file's summary"
        );
        assert!(n["type"].is_string(), "node {n} must carry a type");
        assert_eq!(
            n["via"], "records/contacts/sarah-chen",
            "the one-hop edge must originate at the seed"
        );
    }

    // The company node specifically resolves the right summary/type — proves the
    // wrapper read the reached file's frontmatter, not the seed's.
    let company = nodes
        .iter()
        .find(|n| n["path"] == "records/companies/northstar")
        .expect("company node present");
    assert_eq!(company["type"], "company");
    assert!(company["summary"].as_str().unwrap().contains("175-seat"));
}

#[test]
fn neighborhood_text_is_tab_separated_path_hops_summary() {
    // Text mode is one `path\thops\tsummary` row per node — clean to field-split.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "neighborhood",
        "records/contacts/sarah-chen.md",
        "--hops",
        "1",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let rows = lines(&out);
    assert_eq!(rows.len(), 5, "five one-hop neighbors");
    for row in &rows {
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(
            cols.len(),
            3,
            "row `{row}` must be path<TAB>hops<TAB>summary"
        );
        assert_eq!(cols[1], "1", "hop column is 1 for one-hop neighbors");
        assert!(!cols[2].is_empty(), "summary column is non-empty");
    }
}

#[test]
fn neighborhood_in_layer_filter_narrows_to_that_layer() {
    // `--in wiki` keeps only reached nodes under wiki/: the project page and the
    // sarah-chen bio. The company + meetings (records/) drop out of the RESULT.
    let dir = corpus_a();
    let out = run_ok(&[
        "graph",
        "neighborhood",
        "records/contacts/sarah-chen.md",
        "--hops",
        "1",
        "--in",
        "wiki",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let mut paths: Vec<String> = out
        .lines()
        .map(|l| l.split('\t').next().unwrap().to_string())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "wiki/people/sarah-chen".to_string(),
            "wiki/projects/northstar-renewal".to_string(),
        ]
    );
}

#[test]
fn neighborhood_type_filter_narrows_to_that_type() {
    // `--type meeting` keeps only the two meeting records among the one-hop set.
    let dir = corpus_a();
    let out = run_ok(&[
        "--json",
        "graph",
        "neighborhood",
        "records/contacts/sarah-chen.md",
        "--hops",
        "1",
        "--type",
        "meeting",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let nodes = v["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty(), "at least one meeting neighbor");
    for n in nodes {
        assert_eq!(n["type"], "meeting", "type filter keeps only meetings: {n}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// graph orphans
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn orphans_lists_files_with_no_edges_either_direction() {
    // The canonical corpus is well-wired; the two source emails that announce an
    // AWS invoice / a Marcus intro carry no wiki-links and nothing links to
    // them, so they are the curation worklist. Orphans returns full
    // store-relative paths (with `.md`), sorted.
    let dir = corpus_a();
    let out = run_ok(&["graph", "orphans", "--dir", dir.to_str().unwrap()]);
    assert_eq!(
        lines(&out),
        vec![
            "sources/emails/2026/04/2026-04-28-aws-invoice-available.md",
            "sources/emails/2026/05/2026-05-12-marcus-intro.md",
        ]
    );
}

#[test]
fn orphans_in_layer_scopes_candidates() {
    // Scoped to records/: the canonical records are all wired, so there are no
    // record orphans (the only orphans live in sources/). Empty + exit 0.
    let dir = corpus_a();
    let (code, out, err) = run(&[
        "graph",
        "orphans",
        "--in",
        "records",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr:\n{err}");
    assert!(
        out.trim().is_empty(),
        "no record-layer orphans in the canonical corpus, got:\n{out}"
    );

    // Scoped to sources/: exactly the two source orphans.
    let sources = run_ok(&[
        "graph",
        "orphans",
        "--in",
        "sources",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(
        lines(&sources),
        vec![
            "sources/emails/2026/04/2026-04-28-aws-invoice-available.md",
            "sources/emails/2026/05/2026-05-12-marcus-intro.md",
        ]
    );
}

#[test]
fn orphans_json_is_a_string_array() {
    let dir = corpus_a();
    let out = run_ok(&["--json", "graph", "orphans", "--dir", dir.to_str().unwrap()]);
    let arr: Vec<String> = serde_json::from_str(out.trim()).expect("orphans json array");
    assert_eq!(
        arr,
        vec![
            "sources/emails/2026/04/2026-04-28-aws-invoice-available.md".to_string(),
            "sources/emails/2026/05/2026-05-12-marcus-intro.md".to_string(),
        ]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// tree
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn tree_layer_scope_groups_type_folders_then_files_indented() {
    // `--layer wiki` prints just the wiki branch: the layer line, then each
    // type-folder (people/projects/synthesis) sorted, then its files sorted,
    // indented two spaces per level. Meta files (index.md/index.jsonl) never
    // appear.
    let dir = corpus_a();
    let out = run_ok(&["tree", "--layer", "wiki", "--dir", dir.to_str().unwrap()]);
    assert_eq!(
        lines(&out),
        vec![
            "wiki",
            "  wiki/people",
            "    wiki/people/elena-rodriguez.md",
            "    wiki/people/sarah-chen.md",
            "  wiki/projects",
            "    wiki/projects/northstar-renewal.md",
            "  wiki/synthesis",
            "    wiki/synthesis/2026-renewal-plan.md",
        ]
    );
}

#[test]
fn tree_json_mirrors_the_layer_type_folder_file_structure() {
    let dir = corpus_a();
    let out = run_ok(&[
        "--json",
        "tree",
        "--layer",
        "wiki",
        "--dir",
        dir.to_str().unwrap(),
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("tree json");
    let layers = v["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "only the wiki layer");
    assert_eq!(layers[0]["layer"], "wiki");

    let folders = layers[0]["type_folders"].as_array().unwrap();
    let folder_paths: Vec<&str> = folders
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        folder_paths,
        vec!["wiki/people", "wiki/projects", "wiki/synthesis"]
    );

    let people_files: Vec<&str> = folders[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(
        people_files,
        vec![
            "wiki/people/elena-rodriguez.md",
            "wiki/people/sarah-chen.md"
        ]
    );
}

#[test]
fn tree_type_filter_keeps_only_the_named_type_folder() {
    // `--type contacts` keeps only the records/contacts type-folder and its four
    // contact files.
    let dir = corpus_a();
    let out = run_ok(&["tree", "--type", "contacts", "--dir", dir.to_str().unwrap()]);
    assert_eq!(
        lines(&out),
        vec![
            "records",
            "  records/contacts",
            "    records/contacts/david-kim.md",
            "    records/contacts/elena-rodriguez.md",
            "    records/contacts/marcus-okafor.md",
            "    records/contacts/sarah-chen.md",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// stats
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stats_json_counts_match_the_corpus_shape() {
    // `dbmd stats <DIR>` (DIR is positional on this command). The canonical
    // corpus has 6 source + 505 record + 4 wiki content files = 515, every
    // wiki-link resolves (0 broken — matching EXPECTED/validate.json being
    // empty), and exactly the two source emails are orphans. `expense`
    // dominates the type distribution.
    let dir = corpus_a();
    let out = run_ok(&["--json", "stats", dir.to_str().unwrap()]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stats json");

    assert_eq!(v["total_files"], 515);
    assert_eq!(v["files_per_layer"]["sources"], 6);
    assert_eq!(v["files_per_layer"]["records"], 505);
    assert_eq!(v["files_per_layer"]["wiki"], 4);
    assert_eq!(v["broken_link_count"], 0);
    assert_eq!(v["orphan_count"], 2);

    // `top_types` is an ordered array of [name, count]; expense is #1.
    let top = v["top_types"].as_array().unwrap();
    assert_eq!(top[0][0], "expense");
    assert_eq!(top[0][1], 490);

    // The corpus uses only canonical content types, so `custom_types_present` is
    // empty and the recognized set includes the headline types.
    assert!(v["custom_types_present"].as_array().unwrap().is_empty());
    let recognized: Vec<&str> = v["recognized_types_present"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    for t in ["contact", "company", "email", "expense", "wiki-page"] {
        assert!(
            recognized.contains(&t),
            "{t} should be a recognized type present"
        );
    }
}

#[test]
fn stats_text_reports_totals_and_per_layer_lines() {
    let dir = corpus_a();
    let out = run_ok(&["stats", dir.to_str().unwrap()]);
    assert!(out.contains("files: 515"), "totals line:\n{out}");
    assert!(out.contains("sources: 6"), "per-layer sources:\n{out}");
    assert!(out.contains("records: 505"), "per-layer records:\n{out}");
    assert!(out.contains("wiki: 4"), "per-layer wiki:\n{out}");
    assert!(out.contains("broken links: 0"), "broken-link line:\n{out}");
    assert!(out.contains("orphans: 2"), "orphan line:\n{out}");
}

// ─────────────────────────────────────────────────────────────────────────────
// outline
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn outline_lists_only_h2_plus_sections_with_levels_and_body_lines() {
    // `dbmd outline <FILE>` carries no `--dir`: it opens the store at the CWD, so
    // the test runs the binary FROM the corpus root. The renewal page has a
    // single `#` title (not a section) and two `##` sections — Timeline and
    // Commercials — at body lines 7 and 20.
    let dir = corpus_a();
    let out = run_in(
        &dir,
        &["--json", "outline", "wiki/projects/northstar-renewal.md"],
    );
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("outline json");
    assert_eq!(v["file"], "wiki/projects/northstar-renewal.md");

    let sections = v["sections"].as_array().unwrap();
    let got: Vec<(&str, u64, u64)> = sections
        .iter()
        .map(|s| {
            (
                s["heading"].as_str().unwrap(),
                s["level"].as_u64().unwrap(),
                s["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![("Timeline", 2, 7), ("Commercials", 2, 20)],
        "only ##+ headings; the # title is not a section"
    );
}

#[test]
fn outline_text_indents_by_heading_depth() {
    // Text mode prints each heading indented (level - 2) * 2 spaces; both
    // sections here are level 2, so both are flush-left.
    let dir = corpus_a();
    let out = run_in(&dir, &["outline", "wiki/projects/northstar-renewal.md"]);
    assert_eq!(lines(&out), vec!["Timeline", "Commercials"]);
}

#[test]
fn outline_of_a_file_with_no_h2_sections_is_empty_exit_zero() {
    // A contact record is frontmatter + a single `# Name` title and prose — no
    // `##` sections — so the outline is empty and the command still succeeds.
    let dir = corpus_a();
    let (code, out, err) = run_in_status(&dir, &["outline", "records/contacts/sarah-chen.md"]);
    assert_eq!(code, 0, "stderr:\n{err}");
    assert!(
        out.trim().is_empty(),
        "no ## sections in a contact record:\n{out}"
    );
}

// ── helpers that run the binary with a specific working directory ─────────────

/// Run `dbmd <args...>` with CWD set to `dir`, asserting success and returning
/// stdout. Used by `outline`, whose store is the current directory.
fn run_in(dir: &std::path::Path, args: &[&str]) -> String {
    let (code, out, err) = run_in_status(dir, args);
    assert_eq!(
        code, 0,
        "`dbmd {args:?}` (cwd {dir:?}) exited {code}; stderr:\n{err}"
    );
    out
}

/// Like [`run_in`] but returns the raw `(code, stdout, stderr)`.
fn run_in_status(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dbmd"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn dbmd");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout utf8"),
        String::from_utf8(output.stderr).expect("stderr utf8"),
    )
}
