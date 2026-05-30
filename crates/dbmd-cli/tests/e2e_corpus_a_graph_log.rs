//! End-to-end golden contracts for the **relationship** and **timeline** read
//! axes over the canonical store (`tests/corpora/corpus-a-canonical`), driving
//! the **real `dbmd` binary** as a subprocess (`assert_cmd`). Companion to
//! `e2e_corpus_a.rs` (which owns validate/search/index/round-trip); this file
//! owns the last two corpus-a goldens:
//!
//!   1. **`EXPECTED/graph.json`** — `graph backlinks` + `graph neighborhood`
//!      for representative seeds. The golden is intent-derived (which file
//!      wiki-links which, per the corpus content + SPEC § Linking); this test
//!      AUDITS the tool output against that intent and against the SPEC graph
//!      contract, then asserts the topology equals the golden:
//!        - **backlinks** — for each seed, the `--json` array equals the golden
//!          `matches` exactly (sorted, bare wiki-link form), the text mode
//!          agrees, and NO meta path (`index.md` / `index.jsonl`) ever appears
//!          (an index naming a file is catalog, not a relationship edge).
//!        - **neighborhood** — for each seed/hops case, the reached node set —
//!          keyed `path → {hops, direction, via, type}` — equals the golden
//!          (order-independent, because those four are graph-topology facts, not
//!          enumeration artifacts). Each node's `summary` is non-empty AND
//!          byte-equals the reached file's own `summary:` frontmatter (proving
//!          hydration read the NEIGHBOR, not the seed). Each node's `via` is a
//!          legitimate parent: the seed at hop 1, and a hop-(n-1) node at hop n,
//!          and the claimed `direction` edge between `via` and the node really
//!          exists (cross-checked through the binary's own
//!          `backlinks`/`forwardlinks` primitives). An orphan seed hydrates to
//!          `{seed, nodes:[]}` with exit 0.
//!   2. **`EXPECTED/log-tail.json`** — `log tail 20 --json`, normalized. The
//!      active log holds 12 entries (no rotation), so `tail 20` is the whole
//!      log. The full ordered array (each entry normalized to
//!      `{timestamp, kind, object, note}`) equals the golden; the order is
//!      chronological (oldest first); and the object-slot encoding the SPEC
//!      distinguishes is pinned — a per-file action carries the raw `[[...]]`
//!      form, the store-wide `index-rebuild` carries the literal `"-"`, and the
//!      final `validate` (no object slot) carries JSON `null`.
//!
//! Every expected value lives in the committed JSON goldens; this test is their
//! executable contract. Run after any change that could move graph/log output:
//! `cargo test -p dbmd-cli --test e2e_corpus_a_graph_log`.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{corpus_a, corpus_a_expected, dbmd};

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Run `dbmd --json graph <args> --dir corpus-a` and parse stdout as JSON.
fn graph_json(args: &[&str]) -> serde_json::Value {
    let out = dbmd()
        .arg("--json")
        .arg("graph")
        .args(args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    serde_json::from_str(&stdout).expect("graph --json emits JSON")
}

/// Read one file's `summary:` frontmatter value from the store, with surrounding
/// quotes stripped — the value the neighborhood hydrator must echo for a node
/// whose `path` is `target` (no `.md`). Returns `None` if the file or key is
/// absent. Independent of `dbmd-core` so the audit can't be fooled by a shared
/// bug: it parses the YAML line directly.
fn summary_of(target: &str) -> Option<String> {
    let text = std::fs::read_to_string(corpus_a().join(format!("{target}.md"))).ok()?;
    let (fm, _) = common::split_frontmatter_body(&text)?;
    let raw = fm
        .lines()
        .find_map(|l| l.trim().strip_prefix("summary:").map(str::trim))?;
    let unquoted = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw);
    Some(unquoted.to_string())
}

/// The bare forwardlink targets of `file` (with `.md`), via the binary's own
/// `graph forwardlinks` primitive — used to confirm a neighborhood edge's
/// claimed `direction` against an independent code path.
fn forwardlinks_of(file: &str) -> BTreeSet<String> {
    graph_json(&["forwardlinks", file])
        .as_array()
        .expect("forwardlinks --json is an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// graph.json — golden loading
// ─────────────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct BacklinksCase {
    seed: String,
    matches: Vec<String>,
}

#[derive(serde::Deserialize, Clone)]
struct NeighborNode {
    path: String,
    hops: u64,
    direction: String,
    via: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(serde::Deserialize)]
struct NeighborhoodCase {
    seed: String,
    hops: u64,
    seed_normalized: String,
    nodes: Vec<NeighborNode>,
}

#[derive(serde::Deserialize)]
struct GraphGolden {
    backlinks: Vec<BacklinksCase>,
    neighborhood: Vec<NeighborhoodCase>,
}

fn graph_golden() -> GraphGolden {
    let raw = std::fs::read_to_string(corpus_a_expected("graph.json"))
        .expect("EXPECTED/graph.json is committed");
    serde_json::from_str(&raw).expect("EXPECTED/graph.json is valid JSON")
}

// ─────────────────────────────────────────────────────────────────────────────
// 1 — graph backlinks matches the golden (and excludes catalog/meta edges)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn backlinks_golden_seeds_return_the_expected_incoming_edges() {
    let golden = graph_golden();
    assert!(
        !golden.backlinks.is_empty(),
        "the graph golden has backlinks cases"
    );

    for case in &golden.backlinks {
        // `--json`: the exact sorted bare-form array.
        let json_arr: Vec<String> = graph_json(&["backlinks", &case.seed])
            .as_array()
            .expect("backlinks --json is an array")
            .iter()
            .map(|v| v.as_str().expect("each backlink is a string").to_string())
            .collect();
        assert_eq!(
            json_arr, case.matches,
            "backlinks --json for {} must equal the golden incoming-edge set",
            case.seed
        );

        // AUDIT (SPEC: indexes are catalog, not edges): no meta path may appear.
        for m in &json_arr {
            assert!(
                !m.ends_with("/index") && m != "index" && !m.contains("index.jsonl"),
                "backlinks for {} leaked a catalog/meta path `{m}` — indexes are not relationship edges",
                case.seed
            );
            assert!(
                !m.ends_with(".md"),
                "backlinks must be bare wiki-link form (no .md); got `{m}` for {}",
                case.seed
            );
        }

        // Text mode must agree on the SET (one bare path per line).
        let out = dbmd()
            .args(["graph", "backlinks", &case.seed])
            .arg("--dir")
            .arg(corpus_a())
            .assert()
            .success();
        let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
        let text_set: BTreeSet<String> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let json_set: BTreeSet<String> = json_arr.iter().cloned().collect();
        assert_eq!(
            text_set, json_set,
            "text-mode backlinks for {} must agree with --json on the file set",
            case.seed
        );
    }
}

#[test]
fn backlinks_orphan_seed_is_empty_success() {
    // The golden encodes the orphan seed as an empty `matches`; the binary must
    // exit 0 with no stdout for it (empty result is data, not an error).
    let golden = graph_golden();
    let orphan = golden
        .backlinks
        .iter()
        .find(|c| c.matches.is_empty())
        .expect("the golden includes an orphan backlinks seed");
    let out = dbmd()
        .args(["graph", "backlinks", &orphan.seed])
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    assert!(
        out.get_output().stdout.is_empty(),
        "an orphan seed's backlinks must print nothing"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 — graph neighborhood matches the golden topology + hydrates real summaries
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn neighborhood_golden_cases_match_topology_and_hydrate_neighbor_summaries() {
    let golden = graph_golden();
    assert!(
        !golden.neighborhood.is_empty(),
        "the graph golden has neighborhood cases"
    );

    for case in &golden.neighborhood {
        let v = graph_json(&["neighborhood", &case.seed, "--hops", &case.hops.to_string()]);

        // Seed is echoed in normalized (bare) form.
        assert_eq!(
            v["seed"], case.seed_normalized,
            "neighborhood seed for {} must be the normalized bare path",
            case.seed
        );

        let nodes = v["nodes"]
            .as_array()
            .expect("neighborhood has a nodes array");

        // ── topology: path → (hops, direction, via, type), order-independent ──
        let tool: BTreeMap<String, (u64, String, String, String)> = nodes
            .iter()
            .map(|n| {
                (
                    n["path"].as_str().unwrap().to_string(),
                    (
                        n["hops"].as_u64().unwrap(),
                        n["direction"].as_str().unwrap().to_string(),
                        n["via"].as_str().unwrap().to_string(),
                        n["type"].as_str().unwrap().to_string(),
                    ),
                )
            })
            .collect();
        let want: BTreeMap<String, (u64, String, String, String)> = case
            .nodes
            .iter()
            .map(|n| {
                (
                    n.path.clone(),
                    (n.hops, n.direction.clone(), n.via.clone(), n.ty.clone()),
                )
            })
            .collect();
        assert_eq!(
            tool, want,
            "neighborhood({}, hops={}) topology (path→hops/direction/via/type) must equal the golden",
            case.seed, case.hops
        );

        // The set of hop-1 paths in THIS result — every higher-hop node's `via`
        // must be one of these (or the seed), proving real BFS parent tracking.
        let hop1: BTreeSet<&str> = case
            .nodes
            .iter()
            .filter(|n| n.hops == 1)
            .map(|n| n.path.as_str())
            .collect();

        for n in nodes {
            let path = n["path"].as_str().unwrap();
            let hops = n["hops"].as_u64().unwrap();
            let via = n["via"].as_str().unwrap();
            let direction = n["direction"].as_str().unwrap();

            // hydration: summary present AND equal to the reached file's own
            // frontmatter summary (NOT the seed's) — audited against the file.
            let got_summary = n["summary"].as_str().unwrap_or("");
            assert!(
                !got_summary.is_empty(),
                "node {path} must carry a non-empty summary"
            );
            let file_summary = summary_of(path).unwrap_or_else(|| {
                panic!("node {path} should resolve to a corpus file w/ summary")
            });
            assert_eq!(
                got_summary, file_summary,
                "node {path} must hydrate the NEIGHBOR's own summary, verbatim from its frontmatter"
            );
            assert_ne!(
                got_summary,
                summary_of(&case.seed_normalized).unwrap_or_default(),
                "node {path} must not echo the SEED's summary"
            );

            // via legitimacy: seed at hop 1; a hop-1 node at hop 2.
            if hops == 1 {
                assert_eq!(
                    via, case.seed_normalized,
                    "a hop-1 node ({path}) must be reached via the seed"
                );
            } else {
                assert!(
                    hop1.contains(via),
                    "a hop-{hops} node ({path}) must be reached via a hop-1 node, got via=`{via}`"
                );
            }

            // direction legitimacy: the claimed edge between `via` and `path`
            // really exists, cross-checked through forwardlinks (independent
            // code path). `outgoing` ⇒ via links to path; `incoming` ⇒ path
            // links to via.
            match direction {
                "outgoing" => assert!(
                    forwardlinks_of(&format!("{via}.md")).contains(path),
                    "node {path} claims direction=outgoing via {via}, but {via} has no forwardlink to it"
                ),
                "incoming" => assert!(
                    forwardlinks_of(&format!("{path}.md")).contains(via),
                    "node {path} claims direction=incoming via {via}, but it has no forwardlink to {via}"
                ),
                other => panic!("node {path} has an unrecognized direction `{other}`"),
            }
        }
    }
}

#[test]
fn neighborhood_orphan_seed_hydrates_to_empty() {
    // The golden carries an orphan neighborhood case (empty nodes); the binary
    // must return a well-formed `{seed, nodes:[]}` with exit 0.
    let golden = graph_golden();
    let orphan = golden
        .neighborhood
        .iter()
        .find(|c| c.nodes.is_empty())
        .expect("the golden includes an orphan neighborhood case");
    let v = graph_json(&[
        "neighborhood",
        &orphan.seed,
        "--hops",
        &orphan.hops.to_string(),
    ]);
    assert_eq!(v["seed"], orphan.seed_normalized);
    assert!(
        v["nodes"].as_array().unwrap().is_empty(),
        "an isolated seed hydrates to no nodes"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3 — log tail 20 matches the normalized golden (order + object-slot encoding)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, PartialEq, Debug)]
struct LogEntry {
    timestamp: String,
    kind: String,
    // `object` is a string for per-file + store-wide-`-` actions, JSON null for
    // an action with no object slot (e.g. a bare `validate`).
    object: Option<String>,
    note: String,
}

#[derive(serde::Deserialize)]
struct LogTailGolden {
    tail_n: u64,
    expected_count: usize,
    chronological_oldest_first: bool,
    entries: Vec<LogEntry>,
}

fn log_tail_golden() -> LogTailGolden {
    let raw = std::fs::read_to_string(corpus_a_expected("log-tail.json"))
        .expect("EXPECTED/log-tail.json is committed");
    serde_json::from_str(&raw).expect("EXPECTED/log-tail.json is valid JSON")
}

#[test]
fn log_tail_json_matches_the_normalized_golden() {
    let golden = log_tail_golden();
    assert!(
        golden.chronological_oldest_first,
        "golden documents oldest-first order"
    );

    let out = dbmd()
        .args(["--json", "log", "tail", &golden.tail_n.to_string()])
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let raw: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).expect("log tail --json is an array");

    // Count: tail 20 over a 12-entry log returns all 12.
    assert_eq!(
        raw.len(),
        golden.expected_count,
        "tail {} must return {} entries (the whole active log)",
        golden.tail_n,
        golden.expected_count
    );

    // Normalize each tool entry to the four golden keys, preserving the JSON
    // null-vs-string distinction on `object`.
    let tool: Vec<LogEntry> = raw
        .iter()
        .map(|e| LogEntry {
            timestamp: e["timestamp"]
                .as_str()
                .expect("timestamp string")
                .to_string(),
            kind: e["kind"].as_str().expect("kind string").to_string(),
            object: match &e["object"] {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.clone()),
                other => panic!("log entry `object` must be string or null, got {other}"),
            },
            note: e["note"].as_str().expect("note string").to_string(),
        })
        .collect();

    // Full ordered array equality — order, kinds, notes, AND object encoding.
    assert_eq!(
        tool, golden.entries,
        "log tail --json must equal the normalized golden array, in chronological order"
    );

    // ── AUDIT: chronological non-decreasing timestamps (oldest first) ─────────
    let timestamps: Vec<&str> = tool.iter().map(|e| e.timestamp.as_str()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable(); // `YYYY-MM-DD HH:MM` sorts lexicographically == chronologically
    assert_eq!(
        timestamps, sorted,
        "tail must return entries oldest-first (chronological)"
    );

    // ── AUDIT: the SPEC object-slot encoding distinctions are real ───────────
    // A per-file action carries the raw [[...]] wiki-link form.
    let per_file = tool
        .iter()
        .find(|e| e.kind == "link")
        .expect("the log has a `link` entry");
    let obj = per_file.object.as_deref().unwrap_or("");
    assert!(
        obj.starts_with("[[") && obj.ends_with("]]"),
        "a per-file log object must be the raw wiki-link form, got {obj:?}"
    );
    // The store-wide index-rebuild carries the literal `-` placeholder string.
    let rebuild = tool
        .iter()
        .find(|e| e.kind == "index-rebuild")
        .expect("the log has an `index-rebuild` entry");
    assert_eq!(
        rebuild.object.as_deref(),
        Some("-"),
        "a store-wide `index-rebuild` records its object as the literal `-`"
    );
    // The bare `validate` (no object slot) carries JSON null — distinct from `-`.
    let validate = tool
        .iter()
        .rev()
        .find(|e| e.kind == "validate")
        .expect("the log ends with a `validate` entry");
    assert_eq!(
        validate.object, None,
        "a `validate` entry with no object slot must serialize `object` as null (not `-`)"
    );
}

#[test]
fn log_tail_small_n_returns_newest_window_in_order() {
    // A smaller tail must return the LAST N golden entries, still chronological.
    // This guards the tail window (drop-from-the-front) against an off-by-one or
    // a newest-first regression that the full-log case (N >= len) can't catch.
    let golden = log_tail_golden();
    let n = 3usize;
    assert!(
        golden.entries.len() > n,
        "the golden has more than {n} entries"
    );
    let want: &[LogEntry] = &golden.entries[golden.entries.len() - n..];

    let out = dbmd()
        .args(["--json", "log", "tail", &n.to_string()])
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let raw: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(raw.len(), n, "tail {n} returns exactly {n} entries");

    let tool: Vec<LogEntry> = raw
        .iter()
        .map(|e| LogEntry {
            timestamp: e["timestamp"].as_str().unwrap().to_string(),
            kind: e["kind"].as_str().unwrap().to_string(),
            object: match &e["object"] {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.clone()),
                other => panic!("object must be string or null, got {other}"),
            },
            note: e["note"].as_str().unwrap().to_string(),
        })
        .collect();
    assert_eq!(
        tool, want,
        "tail {n} must be the newest {n} golden entries, oldest-first"
    );
}
