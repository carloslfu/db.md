// SPDX-License-Identifier: Apache-2.0

//! `dbmd rm` — link-aware delete of one content file.
//!
//! Every case runs against a scratch store built through the real write
//! surface (`dbmd write` / `dbmd link`), so the write-through index state
//! `rm` mutates is exactly what production writes produce. Assertions pin
//! properties: exit codes, stable error codes, on-disk survival, index and
//! validate outcomes.

mod common;

use std::path::{Path, PathBuf};

use common::{dbmd, write_file};

/// A scratch store with two flat-type records, `records/widgets/{a,b}.md`,
/// created through `dbmd write` so both index artifacts exist write-through.
fn scratch_store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path().to_path_buf();
    write_file(
        &store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n# T\n",
    );
    for (path, summary) in [
        ("records/widgets/a.md", "Widget A"),
        ("records/widgets/b.md", "Widget B"),
    ] {
        dbmd()
            .args([
                "write",
                path,
                "--type",
                "widget",
                "--summary",
                summary,
                "--dir",
            ])
            .arg(&store)
            .assert()
            .success();
    }
    (dir, store)
}

/// Deleting an unlinked record removes the file and its catalog row; the
/// store stays fully valid.
#[test]
fn rm_unlinked_removes_file_and_index_row() {
    let (_tmp, store) = scratch_store();

    let assert = dbmd()
        .args(["--json", "rm", "records/widgets/a.md", "--dir"])
        .arg(&store)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        out,
        serde_json::json!({
            "removed": "records/widgets/a.md",
            "backlinks": [],
            "forced": false,
        })
    );

    assert!(!store.join("records/widgets/a.md").exists());
    let jsonl = std::fs::read_to_string(store.join("records/widgets/index.jsonl")).unwrap();
    assert!(
        !jsonl.contains("a.md"),
        "catalog row must be dropped: {jsonl}"
    );
    assert!(jsonl.contains("b.md"), "sibling row must survive: {jsonl}");

    dbmd()
        .args(["validate", "--all"])
        .arg(&store)
        .assert()
        .success();
}

/// Deleting the folder's last record also removes both derived index
/// artifacts (an empty type-folder carries no catalog).
#[test]
fn rm_last_record_removes_index_artifacts() {
    let (_tmp, store) = scratch_store();
    for path in ["records/widgets/a.md", "records/widgets/b.md"] {
        dbmd()
            .args(["rm", path, "--dir"])
            .arg(&store)
            .assert()
            .success();
    }
    assert!(!store.join("records/widgets/index.md").exists());
    assert!(!store.join("records/widgets/index.jsonl").exists());
    dbmd()
        .args(["validate", "--all"])
        .arg(&store)
        .assert()
        .success();
}

/// While another content file still wiki-links to the target, `rm` refuses
/// with the stable `RM_LINKED` contract (exit 5) listing each linker, and
/// deletes nothing.
#[test]
fn rm_linked_refuses_and_lists_backlinks() {
    let (_tmp, store) = scratch_store();
    dbmd()
        .args([
            "link",
            "records/widgets/b.md",
            "records/widgets/a.md",
            "--dir",
        ])
        .arg(&store)
        .assert()
        .success();

    let assert = dbmd()
        .args(["--json", "rm", "records/widgets/a.md", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(5); // ExitCode::Collision
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("RM_LINKED"));
    assert_eq!(
        err["error"]["details"]["backlinks"],
        serde_json::json!(["records/widgets/b.md"])
    );

    assert!(
        store.join("records/widgets/a.md").exists(),
        "refusal must not delete"
    );
}

/// `--force` deletes a linked target, reports the now-broken linkers, and
/// the break is exactly what `validate --all` then flags.
#[test]
fn rm_force_deletes_and_validate_flags_broken_links() {
    let (_tmp, store) = scratch_store();
    dbmd()
        .args([
            "link",
            "records/widgets/b.md",
            "records/widgets/a.md",
            "--dir",
        ])
        .arg(&store)
        .assert()
        .success();

    let assert = dbmd()
        .args(["--json", "rm", "records/widgets/a.md", "--force", "--dir"])
        .arg(&store)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        out,
        serde_json::json!({
            "removed": "records/widgets/a.md",
            "backlinks": ["records/widgets/b.md"],
            "forced": true,
        })
    );

    let assert = dbmd()
        .args(["--json", "validate", "--all"])
        .arg(&store)
        .assert()
        .failure()
        .code(6); // ExitCode::ValidationFailed
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("WIKI_LINK_BROKEN"),
        "forced delete must surface as WIKI_LINK_BROKEN: {stdout}"
    );
}

/// A frozen page is never deletable (exit 4, `POLICY_FROZEN_PAGE`).
#[test]
fn rm_frozen_page_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path();
    write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Policies\n\n### Frozen pages\n- `records/widgets/a.md` — signed off.\n",
    );
    dbmd()
        .args([
            "write",
            "records/widgets/a.md",
            "--type",
            "widget",
            "--summary",
            "A",
            "--dir",
        ])
        .arg(store)
        .assert()
        .failure(); // the write surface itself refuses frozen paths
    write_file(
        store,
        "records/widgets/a.md",
        "---\ntype: widget\nsummary: A\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n",
    );

    let assert = dbmd()
        .args(["--json", "rm", "records/widgets/a.md", "--dir"])
        .arg(store)
        .assert()
        .failure()
        .code(4); // ExitCode::Policy
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(
        err["error"]["code"],
        serde_json::json!("POLICY_FROZEN_PAGE")
    );
    assert!(store.join("records/widgets/a.md").exists());
}

/// Reserved meta files are never deletable, wherever the basename sits.
#[test]
fn rm_reserved_meta_refused() {
    let (_tmp, store) = scratch_store();
    for path in [
        "DB.md",
        "log.md",
        "records/widgets/index.md",
        "records/widgets/index.jsonl",
    ] {
        let assert = dbmd()
            .args(["--json", "rm", path, "--dir"])
            .arg(&store)
            .assert()
            .failure()
            .code(4); // ExitCode::Policy
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
        assert_eq!(
            err["error"]["code"],
            serde_json::json!("RM_RESERVED_META"),
            "{path}"
        );
    }
    assert!(store.join("DB.md").exists());
    assert!(store.join("records/widgets/index.md").exists());
}

/// A directory target, a non-content path, and a missing file each get their
/// own stable refusal.
#[test]
fn rm_shape_guards() {
    let (_tmp, store) = scratch_store();

    let assert = dbmd()
        .args(["--json", "rm", "records/widgets", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("RM_NOT_A_FILE"));

    write_file(&store, "scratch/x.md", "loose bytes\n");
    let assert = dbmd()
        .args(["--json", "rm", "scratch/x.md", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("RM_NOT_CONTENT"));
    assert!(store.join("scratch/x.md").exists());

    dbmd()
        .args(["rm", "records/widgets/missing.md", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime
}

/// `rm` runs under the store transaction lock and leaves the lock file
/// behind like every other mutating verb — sanity that the file relocation
/// still works with a relative CWD invocation.
#[test]
fn rm_works_from_inside_the_store() {
    let (_tmp, store) = scratch_store();
    dbmd()
        .current_dir(&store)
        .args(["rm", "records/widgets/a.md"])
        .assert()
        .success();
    assert!(!Path::new(&store).join("records/widgets/a.md").exists());
}
