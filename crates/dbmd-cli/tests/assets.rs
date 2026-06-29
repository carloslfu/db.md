//! Integration tests for `dbmd assets` (scan / verify / status / paths) and the
//! asset-manifest validation codes.
//!
//! These drive the real `dbmd` binary against synthetic temp stores (the asset
//! commands write `assets.jsonl`, so the committed corpora are never touched).
//! Intent-derived: they assert the properties that MUST hold — the manifest is a
//! pure projection of declarations, `verify` is the byte-completeness gate,
//! `validate` checks integrity without reading bytes (so a fresh clone passes),
//! and a hostile asset path can never escape the store.

mod common;

use common::{dbmd, write_db_md, write_file};
use serde_json::Value;

const WRAPPER: &str = "\
---
type: pdf-source
created: 2026-06-17T09:00:00-05:00
updated: 2026-06-17T09:00:00-05:00
summary: \"Contract PDF wrapper\"
asset: sources/docs/2026/06/contract.pdf
---

# Contract
";

/// A store with one wrapper declaring one present binary asset.
fn setup(dir: &std::path::Path) {
    write_db_md(dir);
    write_file(dir, "sources/docs/2026/06/contract.pdf.md", WRAPPER);
    write_file(
        dir,
        "sources/docs/2026/06/contract.pdf",
        "FAKE PDF BYTES 0123456789 abcdefghij",
    );
}

fn json_stdout(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is valid JSON")
}

#[test]
fn scan_catalogs_then_verify_passes() {
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());

    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(v["cataloged"], 1);
    assert_eq!(v["hashed"], 1);
    assert_eq!(v["preserved"], 0);
    assert_eq!(v["wrote"], true);

    // The manifest is a real, parseable JSONL record with a 64-hex digest.
    let manifest = std::fs::read_to_string(tmp.path().join("assets.jsonl")).unwrap();
    let rec: Value = serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
    assert_eq!(rec["path"], "sources/docs/2026/06/contract.pdf");
    assert_eq!(rec["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(rec["media_type"], "application/pdf");
    assert_eq!(rec["required"], true);
    assert_eq!(rec["wrappers"][0], "sources/docs/2026/06/contract.pdf.md");

    // Verify is the gate: present + hash-correct ⇒ complete, exit 0.
    let assert = dbmd()
        .args(["--json", "assets", "verify", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(v["complete"], true);
    assert_eq!(v["checked"], 1);
}

#[test]
fn scan_is_idempotent_no_change_on_second_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(
        v["wrote"], false,
        "a no-op rescan must not rewrite the manifest"
    );
}

#[test]
fn scan_recompacts_duplicate_line_manifest() {
    // The documented git `merge=union` recovery (SPEC § Assets): a manifest with
    // duplicate identical lines must be recompacted to the single canonical line
    // by `assets scan`, and reported as updated — not silently left as-is. The
    // bug was a no-change gate comparing parsed (deduped-by-path) records instead
    // of the on-disk bytes, so a duplicate-line manifest parsed back equal and
    // was never repaired.
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();

    let manifest = tmp.path().join("assets.jsonl");
    let canonical = std::fs::read_to_string(&manifest).unwrap();
    assert_eq!(canonical.lines().count(), 1);

    // Simulate `merge=union`: the same canonical content, twice.
    std::fs::write(&manifest, format!("{canonical}{canonical}")).unwrap();
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap().lines().count(),
        2
    );

    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(
        v["wrote"], true,
        "a non-canonical (duplicate-line) manifest must be recompacted and reported as updated"
    );

    let after = std::fs::read_to_string(&manifest).unwrap();
    assert_eq!(
        after.lines().count(),
        1,
        "duplicate lines must collapse to the single canonical line"
    );
    assert_eq!(
        after, canonical,
        "scan must restore the exact canonical bytes"
    );

    // And re-running over the now-canonical manifest is a true no-op again.
    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(
        v["wrote"], false,
        "a recompacted, canonical manifest must rescan as no-change"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        canonical,
        "the no-op rescan must leave the manifest byte-identical"
    );
}

#[test]
fn verify_fails_and_status_reports_missing_required() {
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    std::fs::remove_file(tmp.path().join("sources/docs/2026/06/contract.pdf")).unwrap();

    // The gate fails (non-zero) when a required asset is absent.
    dbmd()
        .args(["assets", "verify", "--dir"])
        .arg(tmp.path())
        .assert()
        .failure();

    // status never fails; it reports the gap.
    let assert = dbmd()
        .args(["--json", "assets", "status", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(v["missing"], 1);
    assert_eq!(v["required_missing"], 1);
}

#[test]
fn rescan_preserves_evicted_asset() {
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    // Evict the bytes (disk-relief): the record must survive, hash preserved.
    std::fs::remove_file(tmp.path().join("sources/docs/2026/06/contract.pdf")).unwrap();

    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(v["cataloged"], 1);
    assert_eq!(v["preserved"], 1);
    assert_eq!(v["hashed"], 0);
    assert!(std::fs::read_to_string(tmp.path().join("assets.jsonl"))
        .unwrap()
        .contains("contract.pdf"));
}

#[test]
fn traversal_asset_path_is_rejected_and_not_cataloged() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    write_file(
        tmp.path(),
        "sources/docs/2026/06/evil.md",
        "---\ntype: pdf-source\ncreated: 2026-06-17T09:00:00-05:00\nupdated: \
         2026-06-17T09:00:00-05:00\nsummary: \"evil\"\nasset: \
         ../../../../../../etc/passwd\n---\n\n# Evil\n",
    );
    let assert = dbmd()
        .args(["--json", "assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    assert_eq!(v["cataloged"], 0, "a `..` path must never be cataloged");
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("..")),
        "the rejection is reported as a warning: {v}"
    );
    // No manifest written when nothing valid was cataloged.
    assert!(!tmp.path().join("assets.jsonl").exists());
}

#[test]
fn paths_omits_store_escaping_records() {
    // SPEC § Assets > Path safety: `dbmd` enforces store-relative containment
    // "wherever it reads the manifest". A poisoned / hand-edited `assets.jsonl`
    // (the `merge=union`-corruption state the SPEC anticipates) with an absolute
    // and a `..`-traversal recorded path must NOT leak those verbatim out of
    // `assets paths` — a harness pipes that list straight into a `.gitignore`
    // managed block or sync-exclude. The escaping entries are omitted (the list
    // analog of how `verify` counts them corrupt and `status` counts them
    // missing); the legitimate in-store path is emitted unchanged.
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();

    // Append two store-escaping records to the scanned manifest.
    let manifest = tmp.path().join("assets.jsonl");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str(
        "{\"path\":\"../../../../../../etc/passwd\",\"sha256\":\"deadbeef\",\"bytes\":4096,\
\"media_type\":\"text/plain\",\"wrappers\":[\"sources/docs/2026/06/contract.pdf.md\"],\
\"required\":false}\n",
    );
    text.push_str(
        "{\"path\":\"/etc/hosts\",\"sha256\":\"deadbeef\",\"bytes\":4096,\
\"media_type\":\"text/plain\",\"wrappers\":[\"sources/docs/2026/06/contract.pdf.md\"],\
\"required\":false}\n",
    );
    std::fs::write(&manifest, text).unwrap();

    // Text form: only the safe in-store path, never the escaping ones.
    let assert = dbmd()
        .args(["assets", "paths", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("sources/docs/2026/06/contract.pdf"),
        "the legitimate in-store path is still emitted: {stdout:?}"
    );
    assert!(
        !stdout.contains("etc/passwd") && !stdout.contains("etc/hosts"),
        "no store-escaping path may leak from `assets paths`: {stdout:?}"
    );

    // JSON form: same containment — only the safe path in the array.
    let assert = dbmd()
        .args(["--json", "assets", "paths", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    let v = json_stdout(assert.get_output());
    let list = v.as_array().expect("paths --json is an array");
    assert_eq!(
        list,
        &vec![Value::from("sources/docs/2026/06/contract.pdf")],
        "JSON `paths` emits only the safe in-store path"
    );
}

#[test]
fn validate_all_passes_on_a_byteless_fresh_clone() {
    // The load-bearing property: `validate` checks manifest integrity (text
    // only), never byte presence, so a clone whose assets have not been restored
    // still validates.
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    dbmd()
        .args(["index", "rebuild", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    // Simulate a fresh clone: text + manifest present, bytes gone.
    std::fs::remove_file(tmp.path().join("sources/docs/2026/06/contract.pdf")).unwrap();

    dbmd()
        .arg("validate")
        .arg(tmp.path())
        .arg("--all")
        .assert()
        .success();
}

#[test]
fn undeclared_asset_is_flagged_by_validate_until_scanned() {
    let tmp = tempfile::TempDir::new().unwrap();
    setup(tmp.path());
    dbmd()
        .args(["index", "rebuild", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();

    // Wrapper declares contract.pdf but it was never scanned into the manifest.
    let assert = dbmd()
        .args(["--json", "validate"])
        .arg(tmp.path())
        .arg("--all")
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("ASSET_UNDECLARED"),
        "validate --all flags the uncataloged declaration: {stdout}"
    );

    // After a scan it reconciles and validate is clean.
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    dbmd()
        .arg("validate")
        .arg(tmp.path())
        .arg("--all")
        .assert()
        .success();
}

#[test]
fn optional_asset_excluded_from_default_verify() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_db_md(tmp.path());
    write_file(
        tmp.path(),
        "records/expenses/e1.md",
        "---\ntype: expense\ncreated: 2026-06-17T09:00:00-05:00\nupdated: \
         2026-06-17T09:00:00-05:00\nsummary: \"expense + optional receipt\"\nassets:\n  - \
         { path: records/expenses/r1.png, required: false }\n---\n\n# Expense\n",
    );
    write_file(tmp.path(), "records/expenses/r1.png", "PNG BYTES");
    dbmd()
        .args(["assets", "scan", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    // Delete the optional asset.
    std::fs::remove_file(tmp.path().join("records/expenses/r1.png")).unwrap();

    // Default verify ignores optional assets ⇒ still complete.
    dbmd()
        .args(["assets", "verify", "--dir"])
        .arg(tmp.path())
        .assert()
        .success();
    // With --include-optional the missing optional asset fails the gate.
    dbmd()
        .args(["assets", "verify", "--include-optional", "--dir"])
        .arg(tmp.path())
        .assert()
        .failure();
}
