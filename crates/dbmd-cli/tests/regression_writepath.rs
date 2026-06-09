//! Regression tests for the write-path launch-blocking bugs (findings #17,
//! #18, #25). Each test reconstructs the finding's exact trigger and asserts the
//! corrected behavior through the real `dbmd` binary, so the bug can never
//! silently return.
//!
//!   - #17 — `dbmd write --summary` / `dbmd fm init --summary` must NOT silently
//!     hard-truncate an explicit >200-char agent summary (parity with
//!     `dbmd fm set`, which preserves it verbatim).
//!   - #18 — `dbmd write`'s collision guard must be atomic, not TOCTOU: two
//!     concurrent writers to the same resolved path can never both succeed and
//!     silently clobber one another's primary content.
//!   - #25 — `dbmd fm init` on a file that opens a `---` frontmatter fence but
//!     never closes it must refuse with `FM_MALFORMED`, not demote the intended
//!     keys into the body and inject a dangling `---`.

mod common;

use std::process::Command as StdCommand;
use std::sync::{Arc, Barrier};

use common::{copy_store_to_temp, corpus_a, dbmd, write_db_md, write_file};

// ─────────────────────────────────────────────────────────────────────────────
// #17 — explicit `--summary` is collapsed but never truncated.
// ─────────────────────────────────────────────────────────────────────────────

/// A >200-char single-line summary whose meaningful qualifier sits in the tail —
/// the exact shape the pre-fix 200-char truncation silently dropped.
fn long_summary() -> String {
    format!(
        "Director of Operations at Northstar; renewal champion who drove the 175-seat expansion and {}END_QUALIFIER",
        "x".repeat(150)
    )
}

#[test]
fn regression_write_preserves_long_explicit_summary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path();
    write_db_md(store);

    let summary = long_summary();
    assert!(summary.chars().count() > 200, "fixture must exceed the cap");

    dbmd()
        .current_dir(store)
        .args([
            "write",
            "records/contacts/sarah.md",
            "--type",
            "contact",
            "--summary",
            &summary,
        ])
        .assert()
        .success();

    let written = std::fs::read_to_string(store.join("records/contacts/sarah.md")).unwrap();
    // The full agent summary survives — the trailing qualifier (the part a
    // 200-char cut would discard) is on disk.
    assert!(
        written.contains("END_QUALIFIER"),
        "explicit --summary must not be truncated; the tail is missing:\n{written}"
    );
    assert!(
        written.contains(&summary),
        "the full explicit summary must round-trip verbatim:\n{written}"
    );
}

#[test]
fn regression_fm_init_preserves_long_explicit_summary() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    write_file(
        &store,
        "records/contacts/nina-ray.md",
        "---\nname: Nina Ray\nrole: Analyst\n---\n\n# Nina Ray\n",
    );

    let summary = long_summary();
    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "init",
            "records/contacts/nina-ray.md",
            "--summary",
            &summary,
        ])
        .assert()
        .success();

    let written = std::fs::read_to_string(store.join("records/contacts/nina-ray.md")).unwrap();
    assert!(
        written.contains("END_QUALIFIER") && written.contains(&summary),
        "fm init --summary must not truncate the agent's value:\n{written}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// #18 — the collision guard is atomic, not TOCTOU.
// ─────────────────────────────────────────────────────────────────────────────

/// Two concurrent `dbmd write`s targeting the SAME resolved path can never both
/// succeed with different content — that is the silent-clobber the pre-fix
/// `exists()`-then-`rename` window allowed. Exactly one wins (exit 0); the other
/// is refused with `PATH_COLLISION` (exit 5); and the surviving file holds the
/// winner's content, not a torn or overwritten mix. Run many rounds so the
/// race window is actually exercised — the invariant is one-sided (it can fail
/// pre-fix, never post-fix).
#[test]
fn regression_concurrent_write_never_silently_clobbers() {
    let bin = env!("CARGO_BIN_EXE_dbmd");

    for round in 0..40 {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = tmp.path().to_path_buf();
        write_db_md(&store);

        let rel = "records/contacts/sarah.md";
        let barrier = Arc::new(Barrier::new(2));
        let summaries = ["SUMMARY_A", "SUMMARY_B"];

        let handles: Vec<_> = summaries
            .iter()
            .map(|s| {
                let bin = bin.to_string();
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                let summary = s.to_string();
                std::thread::spawn(move || {
                    // Synchronize the two spawns so they hit the resolve→claim
                    // window as close together as possible.
                    barrier.wait();
                    let out = StdCommand::new(&bin)
                        .current_dir(&store)
                        .args([
                            "write",
                            "records/contacts/sarah.md",
                            "--type",
                            "contact",
                            "--summary",
                            &summary,
                        ])
                        .output()
                        .expect("spawn dbmd");
                    (summary, out.status.code())
                })
            })
            .collect();

        let results: Vec<(String, Option<i32>)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        let successes: Vec<&String> = results
            .iter()
            .filter(|(_, code)| *code == Some(0))
            .map(|(s, _)| s)
            .collect();
        let collisions = results.iter().filter(|(_, code)| *code == Some(5)).count();

        // Exactly one writer may win; the other MUST be told PATH_COLLISION
        // (exit 5) rather than silently succeeding and clobbering.
        assert_eq!(
            successes.len(),
            1,
            "round {round}: exactly one concurrent write may succeed, got {results:?}",
        );
        assert_eq!(
            collisions, 1,
            "round {round}: the losing write must report PATH_COLLISION (exit 5), got {results:?}",
        );

        // The surviving file holds the winner's content (no torn write, no
        // silent overwrite of the winner by the loser).
        let written = std::fs::read_to_string(store.join(rel)).unwrap();
        let winner = successes[0];
        assert!(
            written.contains(&format!("summary: {winner}")),
            "round {round}: surviving file must hold the winner's summary `{winner}`:\n{written}",
        );
    }
}

/// A plain pre-existing file is still refused with the structured
/// `PATH_COLLISION` and left byte-for-byte untouched — the atomic claim must not
/// regress the ordinary collision contract.
#[test]
fn regression_write_atomic_claim_still_refuses_existing_file_without_overwrite() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let original =
        "---\ntype: contact\nsummary: ORIGINAL\nname: Sarah\n---\n\n# Sarah\n\nOriginal body.\n";
    write_file(&store, "records/contacts/collide.md", original);

    dbmd()
        .current_dir(&store)
        .args([
            "write",
            "records/contacts/collide.md",
            "--type",
            "contact",
            "--summary",
            "A DIFFERENT SUMMARY",
        ])
        .assert()
        .code(5);

    let after = std::fs::read_to_string(store.join("records/contacts/collide.md")).unwrap();
    assert_eq!(
        after, original,
        "a refused write must not modify the existing file",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// #25 — `fm init` refuses an unterminated frontmatter fence.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn regression_fm_init_refuses_unterminated_frontmatter_fence() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    // Opens a `---` fence but never closes it: the operator INTENDED `name`
    // and `role` as frontmatter. Pre-fix, `fm init` demoted these into the body
    // and injected a stray `---`; now it must refuse.
    let malformed = "---\nname: Tom\nrole: VP\n# Tom\nbody\n";
    let path = write_file(&store, "records/contacts/tom.md", malformed);

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", "records/contacts/tom.md"])
        .assert()
        .failure()
        .code(1);

    // The file is left byte-for-byte untouched — no demotion, no injected fence.
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, malformed,
        "a refused fm init must not rewrite the malformed file",
    );
}

#[test]
fn regression_fm_init_refuses_unterminated_fence_with_structured_code() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    write_file(
        &store,
        "records/contacts/tom-json.md",
        "---\nname: Tom\nrole: VP\nbody\n",
    );

    let out = StdCommand::new(env!("CARGO_BIN_EXE_dbmd"))
        .current_dir(&store)
        .args(["--json", "fm", "init", "records/contacts/tom-json.md"])
        .output()
        .expect("spawn dbmd");
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).expect("json error");
    assert_eq!(
        err["error"]["code"], "FM_MALFORMED",
        "unterminated fence must surface the FM_MALFORMED code, got {err}",
    );
}

/// A genuinely headerless import (no opening fence) must STILL be canonicalized —
/// the #25 fix must not regress the raw-markdown import path into a false
/// refusal.
#[test]
fn regression_fm_init_still_imports_headerless_file() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let raw = "# Tom Vega\n\nNew finance contact.\n";
    write_file(&store, "records/contacts/tom-raw.md", raw);

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", "records/contacts/tom-raw.md"])
        .assert()
        .success();

    let written = std::fs::read_to_string(store.join("records/contacts/tom-raw.md")).unwrap();
    assert!(
        written.starts_with("---\n"),
        "frontmatter added:\n{written}"
    );
    assert!(
        written.contains("type: contact"),
        "type inferred:\n{written}"
    );
    assert!(
        written.ends_with(raw),
        "the raw headerless body must be preserved verbatim:\n{written}"
    );
}
