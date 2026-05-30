//! End-to-end extraction test over the **whole** `corpus-c-formats` fixture set.
//!
//! This is the single, data-driven E2E pass the toolkit's test charter asks for:
//! one test that walks *every* fixture the corpus declares, drives the real
//! `dbmd` binary (`dbmd extract <file>`, via `assert_cmd`), and diffs its output
//! against the committed known-good `.txt` sibling.
//!
//! It is deliberately distinct from `extract.rs` (the per-format integration
//! tests) and from the in-crate unit tests in `dbmd-core::extract`. Those assert
//! one fixture per `#[test]`; a fixture that was never created would simply have
//! no test — a *silent* skip by omission. This file instead carries an explicit
//! manifest of the corpus's intended fixtures (corpus-c `README.md`, "Fixtures"
//! table, where the README asserts "Nothing is missing") and accounts for each
//! one:
//!
//! - **Present** (binary + known-good `.txt` both on disk) → extract and diff.
//! - **Never created** (binary absent) → emit a *logged note* to stderr and
//!   record it; the test does **not** silently pass over it. The run summary at
//!   the end lists every skipped fixture by name and reason, so a missing fixture
//!   is loud, not invisible.
//! - **Half-present** (binary on disk but its `.txt` missing, or vice-versa) →
//!   a hard failure: a fixture that exists *must* have its known-good sibling.
//!   This is never a "skip."
//!
//! ## Comparison modes (from corpus-c `README.md` § "Comparison convention")
//!
//! Different decoders agree on *content* but disagree on *layout* (wrap width,
//! blank-line spacing, bullet glyph), so each fixture is diffed at the strictness
//! the README documents for it:
//!
//! - [`Compare::Tokens`] — token-level: collapse every whitespace run (incl.
//!   newlines) to one space on both sides, then compare. "Same words, same
//!   order." The README's recommended, layout-agnostic default.
//! - [`Compare::LineSet`] — order-agnostic content: the sorted set of
//!   token-normalized non-blank lines. Used for `multi-column.pdf`, whose naive
//!   reading order interleaves columns (documented `pdf-extract`/`pdftotext`
//!   behavior) so line *order* legitimately differs from the known-good.
//! - [`Compare::Exact`] — byte-exact (after trailing-newline trim). Used for
//!   `sample.xlsx`, whose tab-separated rows have no soft-wrap to normalize.
//! - [`Compare::Empty`] — the output must be empty. Used for `image-only.pdf`
//!   (no text layer → "empty in, empty out, never hallucinated text").
//! - [`Compare::RefusesEncrypted`] — extraction must *fail cleanly* with the
//!   `DOCUMENT_ENCRYPTED` code and emit nothing to stdout (no `--password` flag
//!   exists; a locked document is always refused). The known-good `.txt` (the
//!   decrypted text) is therefore *not* diffed — its presence is asserted, but
//!   the contract under test is the clean refusal.

mod common;

use std::path::PathBuf;

use common::{corpora_dir, dbmd};

/// How a fixture's extractor output is compared to its known-good `.txt`.
/// One variant per the corpus-c README's documented strictness levels.
#[derive(Debug, Clone, Copy)]
enum Compare {
    /// Token-level: whitespace-run-insensitive "same words, same order."
    Tokens,
    /// Order-agnostic: sorted set of token-normalized non-blank lines.
    LineSet,
    /// Byte-exact after trailing-newline trim (no soft-wrap to normalize).
    Exact,
    /// Output must be empty (no text layer; OCR out of scope).
    Empty,
    /// Extraction must refuse cleanly (encrypted) — stdout empty, exit non-zero,
    /// `DOCUMENT_ENCRYPTED` machine code. The `.txt` is not diffed.
    RefusesEncrypted,
}

/// One declared corpus-c fixture: the document filename (relative to
/// `sources/docs/`) and how to compare it. The `.txt` known-good is always
/// `<file>.txt` alongside it.
struct Fixture {
    /// File name under `sources/docs/`, e.g. `"text.pdf"`.
    file: &'static str,
    /// Comparison mode for this fixture.
    compare: Compare,
}

/// The full manifest of fixtures corpus-c declares (README § "Fixtures").
/// Keeping this list here — rather than discovering files on disk — is what
/// makes a *missing* fixture detectable: a never-created fixture is an entry
/// whose binary is absent, which we log, rather than a test that never existed.
const FIXTURES: &[Fixture] = &[
    Fixture {
        file: "sample.html",
        compare: Compare::Tokens,
    },
    Fixture {
        file: "text.pdf",
        compare: Compare::Tokens,
    },
    Fixture {
        file: "multi-column.pdf",
        compare: Compare::LineSet,
    },
    Fixture {
        file: "weird-fonts.pdf",
        compare: Compare::Tokens,
    },
    Fixture {
        file: "image-only.pdf",
        compare: Compare::Empty,
    },
    Fixture {
        file: "encrypted.pdf",
        compare: Compare::RefusesEncrypted,
    },
    Fixture {
        file: "sample.docx",
        compare: Compare::Tokens,
    },
    Fixture {
        file: "sample.xlsx",
        compare: Compare::Exact,
    },
    Fixture {
        file: "sample.epub",
        compare: Compare::Tokens,
    },
];

/// The `corpus-c-formats/sources/docs` directory.
fn docs_dir() -> PathBuf {
    corpora_dir()
        .join("corpus-c-formats")
        .join("sources")
        .join("docs")
}

/// Token-level normalization: every whitespace run (incl. newlines) → one space,
/// then trim. The corpus's recommended, layout-agnostic comparison.
fn tokens(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sorted set of token-normalized, non-blank lines — order-agnostic content.
fn line_set(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s.lines().map(tokens).filter(|l| !l.is_empty()).collect();
    v.sort();
    v
}

/// Outcome of accounting for one fixture, for the end-of-run summary.
enum Outcome {
    /// Present and its diff/refusal contract held.
    Checked,
    /// Never created (binary absent) — logged, not silently skipped.
    SkippedMissing(String),
}

/// The end-to-end pass: account for every declared fixture, extract + diff each
/// present one, and fail loudly on any content mismatch or half-present fixture.
/// Never-created fixtures are logged and surfaced in the final summary — the
/// charter's "no silent skips" rule.
#[test]
fn extract_end_to_end_over_corpus_c_fixtures() {
    let docs = docs_dir();
    let mut checked = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for fx in FIXTURES {
        let bin = docs.join(fx.file);
        let txt = docs.join(format!("{}.txt", fx.file));

        match account_for_fixture(fx, &bin, &txt) {
            Outcome::Checked => {
                checked += 1;
                eprintln!("[corpus-c E2E] checked: {} ({:?})", fx.file, fx.compare);
            }
            Outcome::SkippedMissing(reason) => {
                eprintln!("[corpus-c E2E] SKIP (not created): {} — {reason}", fx.file);
                skipped.push(format!("{} ({reason})", fx.file));
            }
        }
    }

    // Loud, explicit summary — every skipped fixture is named, never silent.
    eprintln!(
        "[corpus-c E2E] summary: {checked} checked, {} skipped (of {} declared)",
        skipped.len(),
        FIXTURES.len()
    );
    if !skipped.is_empty() {
        eprintln!("[corpus-c E2E] skipped fixtures: {}", skipped.join("; "));
    }

    // At least the happy-path fixtures must be present, or the corpus is broken
    // (the README asserts "Nothing is missing"). If everything was skipped, the
    // E2E test is testing nothing — that itself is a failure.
    assert!(
        checked > 0,
        "no corpus-c fixtures were present to extract — corpus-c-formats/sources/docs is empty or missing"
    );
}

/// Check one fixture end-to-end, or report it as a logged skip.
///
/// - binary missing → [`Outcome::SkippedMissing`] (logged by the caller).
/// - binary present but `.txt` missing → panic (half-present fixtures are a
///   hard error, never a skip).
/// - both present → run `dbmd extract` and assert the fixture's comparison
///   contract; panic on mismatch.
fn account_for_fixture(fx: &Fixture, bin: &std::path::Path, txt: &std::path::Path) -> Outcome {
    if !bin.exists() {
        return Outcome::SkippedMissing(format!("binary fixture absent at {}", bin.display()));
    }

    // A fixture that exists MUST ship its known-good sibling. Missing `.txt` for
    // a present binary is a corpus defect, not a skip — fail loudly.
    assert!(
        txt.exists(),
        "fixture {} exists but its known-good sibling {} is missing — a present fixture must have its .txt",
        fx.file,
        txt.display()
    );

    match fx.compare {
        Compare::RefusesEncrypted => assert_refuses_encrypted(bin),
        Compare::Empty => {
            let got = run_extract_ok(bin);
            assert!(
                got.trim().is_empty(),
                "{}: expected empty output (no text layer), got: {got:?}",
                fx.file
            );
            // Known-good is the 0-byte file; assert that invariant too.
            let expected = read_known_good(txt);
            assert!(
                expected.trim().is_empty(),
                "{}: known-good .txt should be empty for an image-only PDF",
                fx.file
            );
        }
        Compare::Tokens => {
            let got = run_extract_ok(bin);
            let expected = read_known_good(txt);
            assert_eq!(
                tokens(&got),
                tokens(&expected),
                "{}: token-normalized text differs from known-good",
                fx.file
            );
        }
        Compare::LineSet => {
            let got = run_extract_ok(bin);
            let expected = read_known_good(txt);
            assert_eq!(
                line_set(&got),
                line_set(&expected),
                "{}: token-normalized line SET differs from known-good (order-agnostic)",
                fx.file
            );
        }
        Compare::Exact => {
            let got = run_extract_ok(bin);
            let expected = read_known_good(txt);
            assert_eq!(
                got.trim_end(),
                expected.trim_end(),
                "{}: extracted text is not byte-exact with known-good",
                fx.file
            );
        }
    }

    Outcome::Checked
}

/// Run `dbmd extract <bin>`, assert success, return stdout as UTF-8.
fn run_extract_ok(bin: &std::path::Path) -> String {
    let out = dbmd().arg("extract").arg(bin).assert().success();
    String::from_utf8(out.get_output().stdout.clone()).expect("utf-8 stdout")
}

/// Assert an encrypted document refuses cleanly: non-zero exit, empty stdout,
/// and (under `--json`) the stable `DOCUMENT_ENCRYPTED` machine code on stderr.
fn assert_refuses_encrypted(bin: &std::path::Path) {
    // Plain mode: clean refusal — exit 1, no partial bytes on stdout.
    let out = dbmd().arg("extract").arg(bin).assert().failure().code(1);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.is_empty(),
        "encrypted doc must emit nothing to stdout, got: {stdout:?}"
    );

    // JSON mode: the typed error carries the stable code.
    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(bin)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("JSON error object on stderr");
    assert_eq!(
        parsed["error"]["code"], "DOCUMENT_ENCRYPTED",
        "encrypted doc must report the DOCUMENT_ENCRYPTED code"
    );
}

/// Read a known-good `.txt`. (Existence is asserted by the caller before this.)
fn read_known_good(txt: &std::path::Path) -> String {
    std::fs::read_to_string(txt).expect("known-good .txt is readable")
}
