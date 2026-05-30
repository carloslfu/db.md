//! Integration tests for `dbmd extract <file>`.
//!
//! `extract` decodes a binary/markup document to plain text. These tests drive
//! the real `dbmd` binary (`assert_cmd`) against the committed
//! `corpus-c-formats` fixtures, each of which has a known-good `.txt` sibling.
//!
//! Intent-derived (per the corpora `README`s): assertions check the properties
//! that MUST hold — exit codes, the `{text, metadata}` JSON shape, that the
//! extracted *content* matches the known-good text, that an encrypted document
//! refuses cleanly, that a no-text-layer scan yields nothing — rather than
//! byte-snapshotting one crate's exact whitespace. Text comparisons use the
//! corpus's recommended token-level normalization (collapse whitespace runs),
//! which is layout-agnostic and is how the `.txt` expectations were verified.

mod common;

use std::path::PathBuf;

use common::{corpora_dir, dbmd};

/// Absolute path to a `corpus-c-formats` document fixture under `sources/docs/`.
fn fixture(name: &str) -> PathBuf {
    corpora_dir()
        .join("corpus-c-formats")
        .join("sources")
        .join("docs")
        .join(name)
}

/// The known-good `.txt` sibling of a fixture.
fn expected(name: &str) -> String {
    std::fs::read_to_string(fixture(&format!("{name}.txt"))).expect("known-good .txt exists")
}

/// Token-level normalization: every run of whitespace (incl. newlines) → one
/// space, then trim. The corpus's recommended, layout-agnostic comparison.
fn tokens(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run `dbmd extract <fixture>` and return stdout as a String, asserting success.
fn extract_stdout(name: &str) -> String {
    let out = dbmd().arg("extract").arg(fixture(name)).assert().success();
    String::from_utf8(out.get_output().stdout.clone()).expect("utf-8 stdout")
}

// ── happy-path content, one per format ────────────────────────────────────────

#[test]
fn text_pdf_content_matches_known_good() {
    assert_eq!(
        tokens(&extract_stdout("text.pdf")),
        tokens(&expected("text.pdf"))
    );
}

#[test]
fn weird_fonts_pdf_content_matches_known_good() {
    assert_eq!(
        tokens(&extract_stdout("weird-fonts.pdf")),
        tokens(&expected("weird-fonts.pdf"))
    );
}

#[test]
fn docx_content_matches_known_good() {
    assert_eq!(
        tokens(&extract_stdout("sample.docx")),
        tokens(&expected("sample.docx"))
    );
}

#[test]
fn xlsx_content_matches_known_good() {
    // Exact (tab-separated, integers without `.0`) — no soft-wrap to normalize.
    let got = extract_stdout("sample.xlsx");
    assert_eq!(got.trim_end(), expected("sample.xlsx").trim_end());
}

#[test]
fn epub_content_matches_known_good() {
    assert_eq!(
        tokens(&extract_stdout("sample.epub")),
        tokens(&expected("sample.epub"))
    );
}

#[test]
fn html_content_matches_known_good() {
    assert_eq!(
        tokens(&extract_stdout("sample.html")),
        tokens(&expected("sample.html"))
    );
}

#[test]
fn multi_column_pdf_content_present_order_agnostic() {
    // pdf-extract reads column-by-column; the `.txt` captures interleaved
    // (pdftotext) order. Same content, different line order — assert the
    // token-normalized line SET matches (README § multi-column).
    let sort_lines = |s: &str| {
        let mut v: Vec<String> = s.lines().map(tokens).filter(|l| !l.is_empty()).collect();
        v.sort();
        v
    };
    assert_eq!(
        sort_lines(&extract_stdout("multi-column.pdf")),
        sort_lines(&expected("multi-column.pdf"))
    );
}

// ── empty / refusal / error contracts ─────────────────────────────────────────

#[test]
fn image_only_pdf_yields_no_text() {
    // No text layer → empty output (OCR out of scope). Success, empty stdout.
    let got = extract_stdout("image-only.pdf");
    assert!(
        got.trim().is_empty(),
        "image-only PDF must yield no text, got: {got:?}"
    );
}

#[test]
fn encrypted_pdf_without_password_refuses_cleanly() {
    // Locked document: non-zero exit, no partial bytes on stdout (clean refusal).
    let out = dbmd()
        .arg("extract")
        .arg(fixture("encrypted.pdf"))
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.is_empty(),
        "an encrypted doc must emit nothing to stdout, got: {stdout:?}"
    );
}

#[test]
fn encrypted_pdf_json_error_carries_stable_code() {
    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(fixture("encrypted.pdf"))
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("JSON error object");
    assert_eq!(parsed["error"]["code"], "DOCUMENT_ENCRYPTED");
}

#[test]
fn unsupported_extension_is_error_with_stable_code() {
    let tmp = tempfile::TempDir::new().unwrap();
    let txt = tmp.path().join("note.txt");
    std::fs::write(&txt, "plain text, not a supported document").unwrap();

    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(&txt)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("JSON error object");
    assert_eq!(parsed["error"]["code"], "UNSUPPORTED_FORMAT");
}

#[test]
fn missing_file_is_runtime_error_nonzero_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("nope.pdf");
    dbmd()
        .arg("extract")
        .arg(&missing)
        .assert()
        .failure()
        .code(1);
}

// ── --json shape ──────────────────────────────────────────────────────────────

#[test]
fn json_emits_text_and_metadata_shape() {
    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(fixture("sample.xlsx"))
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Shape: { "text": "...", "metadata": { "format": ..., ... } }.
    assert!(parsed.get("text").and_then(|t| t.as_str()).is_some());
    assert_eq!(parsed["metadata"]["format"], "spreadsheet");
    assert_eq!(parsed["metadata"]["sheets"], 1);
    // The text carries the actual cell content.
    let text = parsed["text"].as_str().unwrap();
    assert!(text.contains("Acme Cloud") && text.contains("1200"));
}

#[test]
fn json_pdf_metadata_reports_format_and_pages() {
    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(fixture("text.pdf"))
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(parsed["metadata"]["format"], "pdf");
    assert_eq!(parsed["metadata"]["pages"], 1);
}

// ── --out file sink ───────────────────────────────────────────────────────────

#[test]
fn out_flag_writes_text_to_file_not_stdout() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dest = tmp.path().join("extracted.txt");

    let out = dbmd()
        .arg("extract")
        .arg(fixture("sample.docx"))
        .arg("--out")
        .arg(&dest)
        .assert()
        .success();

    // Nothing on stdout when --out is used.
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.is_empty(),
        "--out must suppress stdout, got: {stdout:?}"
    );

    // The file holds the extracted text.
    let written = std::fs::read_to_string(&dest).expect("--out file was written");
    assert_eq!(tokens(&written), tokens(&expected("sample.docx")));
}

#[test]
fn out_flag_with_json_writes_json_object_to_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dest = tmp.path().join("out.json");

    dbmd()
        .arg("--json")
        .arg("extract")
        .arg(fixture("sample.html"))
        .arg("--out")
        .arg(&dest)
        .assert()
        .success();

    let written = std::fs::read_to_string(&dest).expect("--out json file was written");
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid JSON in file");
    assert_eq!(parsed["metadata"]["format"], "html");
    assert!(parsed["text"]
        .as_str()
        .unwrap()
        .contains("Quarterly Operations Summary"));
}
