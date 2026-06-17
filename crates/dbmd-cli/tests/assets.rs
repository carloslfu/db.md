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
