//! Regression tests for the post-review follow-up fixes (applied after the
//! parallel fix workflow, to close the gaps its per-file verifiers flagged):
//!
//!   - **finding #16 (CLI wiring):** `dbmd graph neighborhood` must route
//!     `--limit` (and a default cap when unset) into the bounded
//!     `neighborhood_capped` traversal, not just a post-hoc output `.take()`.
//!     The perf property (work avoided) is locked by the core test
//!     `neighborhood_capped_bounds_traversal_not_just_output`; this guards that
//!     the wired CLI still returns correct, bounded, closest-first output and
//!     that the default cap does not truncate an ordinary small neighborhood.
//!   - **finding #9 (sibling in search.rs):** `dbmd search` must not abort on a
//!     single invalid UTF-8 byte on a matched line (the `UTF8` sink surfaced an
//!     io::Error → SEARCH_FAILED, discarding every match found so far; the fix
//!     is the lossy sink).
//!
//! These drive the real `dbmd` binary. #16 uses the committed read-only
//! `corpus-a-canonical` fixture; #9 builds a byte-exact synthetic store in a
//! tempdir so the invalid UTF-8 is genuine.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `corpus-a-canonical`, resolved CWD-independently from the crate manifest.
fn corpus_a() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpora/corpus-a-canonical")
        .canonicalize()
        .expect("corpus-a-canonical must exist")
}

/// Run `dbmd <args...>` with `dir` as the working directory (so the default
/// store root is `dir`). Returns `(exit_code, stdout, stderr)`.
fn run_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dbmd"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to spawn dbmd");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// ── finding #16: neighborhood --limit bounds the result via the capped path ──

#[test]
fn neighborhood_default_cap_does_not_truncate_small_neighborhood() {
    let dir = corpus_a();
    // No `--limit`: sarah-chen has exactly 5 one-hop neighbors; the default cap
    // (200) must not truncate them. If the CLI were wired to pass a too-small or
    // zero default, this drops below 5.
    let (code, out, err) = run_in(
        &dir,
        &[
            "--json",
            "graph",
            "neighborhood",
            "records/contacts/sarah-chen.md",
            "--hops",
            "1",
        ],
    );
    assert_eq!(code, 0, "neighborhood must succeed; stderr: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("neighborhood json");
    assert_eq!(
        v["nodes"].as_array().expect("nodes array").len(),
        5,
        "the default traversal cap must not truncate a 5-node neighborhood"
    );
}

#[test]
fn neighborhood_limit_bounds_result_through_capped_path() {
    let dir = corpus_a();
    // `--limit 2`: exactly two nodes come back, each a real one-hop neighbor
    // with a `via` edge back to the seed. The call now goes through
    // `neighborhood_capped(Some(2))`, so the cap bounds the traversal itself;
    // this locks that the wired command still returns correct bounded output.
    let (code, out, err) = run_in(
        &dir,
        &[
            "--json",
            "graph",
            "neighborhood",
            "records/contacts/sarah-chen.md",
            "--hops",
            "1",
            "--limit",
            "2",
        ],
    );
    assert_eq!(code, 0, "neighborhood --limit must succeed; stderr: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("neighborhood json");
    let nodes = v["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "--limit 2 must bound the result to 2 nodes");
    for n in nodes {
        assert_eq!(n["hops"], 1, "each kept node is a one-hop neighbor: {n}");
        assert_eq!(
            n["via"], "records/contacts/sarah-chen",
            "the kept edge must originate at the seed: {n}"
        );
    }
}

// ── finding #9 (sibling in search.rs): invalid UTF-8 must not abort search ───

#[test]
fn search_tolerates_invalid_utf8_on_a_matched_line() {
    let store = tempfile::tempdir().expect("tempdir");
    let root = store.path();

    std::fs::write(
        root.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Tester\ncomputer_id: t\n---\n# test store\n",
    )
    .expect("write DB.md");
    let notes = root.join("records").join("notes");
    std::fs::create_dir_all(&notes).expect("mkdir records/notes");

    // A content file whose matched body line carries an invalid UTF-8 byte
    // (0xFF), with the search term on that same line. Written as raw bytes so
    // the file is genuinely not valid UTF-8 (no NUL, so it is not classed as
    // binary and the scan reaches the line).
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(
        b"---\ntype: note\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\nsummary: a note\n---\n",
    );
    bytes.extend_from_slice(b"the term MSAuniqueterm sits next to a bad byte ");
    bytes.push(0xFF);
    bytes.extend_from_slice(b" on this line\n");
    std::fs::write(notes.join("bad.md"), &bytes).expect("write bad.md");

    let (code, out, err) = run_in(root, &["search", "MSAuniqueterm"]);
    assert_eq!(
        code, 0,
        "search must not fail on a single invalid UTF-8 byte on a matched line; stderr: {err}"
    );
    assert!(
        out.contains("MSAuniqueterm") && out.contains("bad.md"),
        "the match must still be returned (lossily decoded), got: {out:?}"
    );
}
